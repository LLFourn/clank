use std::{collections::HashMap, path::Path};

use maud::{DOCTYPE, Markup, PreEscaped, html};

use crate::daemon::diff_parser::{DiffLineKind as ParsedDiffLineKind, FileDiff, FileDiffMode};
use crate::daemon::service::FeedbackFileSnapshot;
use crate::domain::{FeedbackFileStatus, FeedbackKind};
use crate::review_state::{ReviewGateDecision, ReviewGateState, ReviewPhase, ReviewVerdict};
use crate::storage::events::Event;
use crate::storage::implementation_revisions::ImplementationRevision;
use crate::storage::plan_revisions::PlanRevision;
use crate::storage::plans::Plan;
use crate::storage::sessions::Session;

use super::ui_styles::STYLE;

// ---------- Home (session list) ----------

pub struct SessionRow {
    pub session: Session,
    pub active_plan_state: Option<String>,
    pub review_gate: Option<ReviewGateDecision>,
    pub is_repo_effective: bool,
    /// True when no active plan exists but the most-recent plan for this
    /// session is in state `finished`. Drives the table's `finished`
    /// status chip.
    pub finished: bool,
}

pub struct HomeActivity {
    pub rows: Vec<TimelineRow>,
    pub unavailable: bool,
}

pub fn home(rows: &[SessionRow], activity: &HomeActivity) -> Markup {
    layout(
        "Trinity",
        html! {
            (timeline_head_scripts())
            div.home-header {
                h1 { "Sessions" }
                (sound_test_button())
            }
            table.sessions {
                thead {
                    tr {
                        th { "Session" }
                        th { "Plan" }
                        th { "Repo" }
                        th { "Status" }
                        th { "Updated" }
                        th.row-actions-th { }
                    }
                }
                tbody id="sessions-table-body" {
                    @for row in rows {
                        (session_table_row(row))
                    }
                }
            }
            @if rows.is_empty() {
                p.empty {
                    "No sessions yet. Drop a plan file into "
                    code { ".trinity/plans/" }
                    " or call "
                    code { "register_plan_file" }
                    " from an agent."
                }
            }
            @if activity.unavailable {
                p.activity-warning { "Activity feed unavailable." }
            }
            @if !activity.rows.is_empty() {
                section.timeline-section.home-activity {
                    div.section-title-row {
                        h2 { "Recent activity" }
                    }
                    div.timeline-wrap {
                        section id="home-timeline-feed"
                                class="timeline-feed"
                                data-timeline-feed=""
                                hx-ext="sse"
                                sse-connect="/events"
                                sse-swap="message"
                                hx-swap="none" {
                            @for row in &activity.rows {
                                (timeline_row_article(row, false))
                            }
                        }
                    }
                }
            }
        },
    )
}

/// Wrap a session row in an htmx OOB `afterbegin` insert against the
/// homepage sessions-table tbody. Used for `session_reactivated` and
/// "first plan revision" SSE events.
pub fn session_table_row_insert(row: &SessionRow) -> Markup {
    html! {
        template hx-swap-oob="afterbegin:#sessions-table-body" {
            (session_table_row_live(row, true))
        }
    }
}

/// Wrap a session row in an htmx OOB `outerHTML` swap targeting the
/// existing row by id. Used for in-place state changes
/// (state_transition, plan_finished, feedback_* events).
pub fn session_table_row_replace(row: &SessionRow) -> Markup {
    let session_id = row.session.id.clone();
    html! {
        template hx-swap-oob={ "outerHTML:#session-row-" (session_id) } {
            (session_table_row_live(row, true))
        }
    }
}

/// Emit a self-targeting OOB `<tr hx-swap-oob="delete">` that htmx
/// removes from the DOM. Used for `session_archived` events.
pub fn session_table_row_remove(session_id: &str) -> Markup {
    html! {
        tr id={ "session-row-" (session_id) } hx-swap-oob="delete" { }
    }
}

fn session_table_row_live(row: &SessionRow, live: bool) -> Markup {
    let session_id = row.session.id.clone();
    let plan_basename = Path::new(&row.session.plan_file_path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(row.session.plan_file_path.as_str())
        .to_string();
    let chip = status_chip(row);
    let class = if live {
        "session-row live"
    } else {
        "session-row"
    };
    let can_finish = matches!(
        row.active_plan_state.as_deref(),
        Some("planning") | Some("implementing")
    );
    let can_claim = can_finish && !row.is_repo_effective;
    html! {
        tr id={ "session-row-" (session_id) } class=(class) {
            td.mono { a href={ "/sessions/" (session_id) } { (session_id) } }
            td.plan-cell { code.mono { (plan_basename) } }
            td.path { (row.session.repo_root) }
            td { (chip) }
            td.relative { (relative_time_tag(Some(row.session.updated_at))) }
            td.row-actions {
                @if can_finish {
                    @if can_claim {
                        form method="post" action={ "/sessions/" (session_id) "/claim" } class="row-claim-form" {
                            button type="submit"
                                    class="row-action-button"
                                    title="Set this session as the repo's implementation target"
                                    aria-label="Set this session in effect" {
                                (action_icon(ActionIcon::Check))
                            }
                        }
                    }
                    form method="post" action={ "/sessions/" (session_id) "/finish" } class="row-finish-form" {
                        button type="submit"
                                class="row-action-button success"
                                title="Mark plan as finished"
                                aria-label="Mark plan as finished"
                                onclick="return confirm('Mark this session as finished? The active plan will transition to `finished`.');" {
                            (action_icon(ActionIcon::Check))
                        }
                    }
                }
                form method="post" action={ "/sessions/" (session_id) "/delete" } class="row-delete-form" {
                    button type="submit"
                            class="row-action-button danger"
                            title="Delete session"
                            aria-label="Delete session"
                            onclick="return confirm('Delete this session? Plan file and feedback files are unchanged. Re-register to restore.');" {
                        (action_icon(ActionIcon::Delete))
                    }
                }
            }
        }
    }
}

pub fn session_table_row(row: &SessionRow) -> Markup {
    session_table_row_live(row, false)
}

fn status_chip(row: &SessionRow) -> Markup {
    let override_label = row
        .review_gate
        .as_ref()
        .and_then(|gate| gate.override_status.as_ref())
        .map(|o| format!("overridden by {}", o.actor));
    let (text, class) = match (
        row.active_plan_state.as_deref(),
        row.review_gate.as_ref().map(|g| g.state),
    ) {
        (Some("planning"), Some(ReviewGateState::Ready)) => {
            ("ready to implement", "status-chip ready")
        }
        (Some("planning"), Some(ReviewGateState::ChangesRequested)) => {
            ("changes requested", "status-chip blocked")
        }
        (Some("planning"), _) => ("planning", "status-chip planning"),
        (Some("implementing"), Some(ReviewGateState::Ready)) => {
            ("ready to finish", "status-chip ready")
        }
        (Some("implementing"), Some(ReviewGateState::ChangesRequested)) => {
            ("impl changes requested", "status-chip blocked")
        }
        (Some("implementing"), _) => ("implementing", "status-chip implementing"),
        (Some("archived"), _) => ("archived", "status-chip muted"),
        (None, _) if row.finished => ("finished", "status-chip finished"),
        _ => ("-", "status-chip muted"),
    };
    html! {
        span class=(class) title=[override_label] {
            (text)
            @if row.is_repo_effective {
                " · in effect"
            }
            @if row.review_gate.as_ref().and_then(|g| g.override_status.as_ref()).is_some() {
                " *"
            }
        }
    }
}

// ---------- Session detail (active plan + history summary) ----------

pub struct FeedbackItem {
    pub author_label: String,
    pub body: String,
    pub verdict: ReviewVerdict,
    pub created_at: i64,
    pub updated_at: i64,
    pub target_label: Option<String>,
}

pub struct SessionDetail {
    pub session: Session,
    pub active_plan: Option<Plan>,
    pub is_repo_effective: bool,
    pub active_plan_preview: Option<ActivePlanPreview>,
    pub archived_count: i64,
    /// Pre-derived snapshots from `SessionService::build_feedback_context`.
    pub plan_feedback_files: Vec<FeedbackFileSnapshot>,
    pub impl_feedback_files: Vec<FeedbackFileSnapshot>,
    pub git_logs_head_path: Option<String>,
    pub plan_feedback_dir_path: Option<String>,
    pub impl_feedback_dir_path: Option<String>,
    pub plan_review_gate: Option<ReviewGateDecision>,
    pub impl_review_gate: Option<ReviewGateDecision>,
    pub timeline_rows: Vec<TimelineRow>,
    /// The maximum event id at render time, embedded in the SSE
    /// `?since=` so the live stream picks up from where the page rendered.
    pub max_event_id: i64,
}

pub struct ActivePlanPreview {
    pub rev_id: i64,
    pub revision_number: i64,
    pub body_html: String,
    pub created_at: i64,
    pub is_long: bool,
}

pub struct DiffViewFeedback {
    pub feedback_id: i64,
    pub author_label: String,
    pub feedback_kind: String,
    pub verdict: ReviewVerdict,
    pub file_status: Option<FeedbackFileStatus>,
    pub file_path: Option<String>,
    pub body_html: String,
    pub created_at: i64,
}

pub struct CommitDiffView {
    pub session_id: String,
    pub commit: ImplementationRevision,
    pub diff: String,
    pub files: Vec<FileDiff>,
    /// Which base SHA the diff was computed against. Rendered prominently in
    /// the sticky header — `Parent(None)` means root commit (no parent).
    pub base_label: super::http::DiffBaseLabel,
    pub feedback: Vec<DiffViewFeedback>,
    pub is_amend: bool,
}

#[derive(Debug, Clone)]
pub struct TimelineRow {
    pub event_id: i64,
    pub event_kind: String,
    pub session_id_for_prefix: Option<String>,
    pub session_title_for_prefix: Option<String>,
    pub actor: String,
    pub ts: i64,
    pub title: TimelineTitle,
    pub preview: Option<String>,
    pub actions: Vec<TimelineAction>,
    pub status_badges: Vec<StatusBadge>,
    pub needs_attention: NeedsAttention,
    pub kind_class: &'static str,
}

#[derive(Debug, Clone)]
pub enum TimelineTitle {
    PlanRevision { revision_number: i64 },
    Commit { short_sha: String },
    HeadReset { short_sha: String },
    Feedback { kind: FeedbackKind, author: String },
    ReviewGate { phase: ReviewPhase },
    StateTransition,
    SessionClaimed,
    AgentJoined { actor: String },
    Warning { kind: WarningKind },
    Other { kind: String },
}

#[derive(Debug, Clone)]
pub enum WarningKind {
    DirtyWorktree,
    PlanFileMissing,
}

#[derive(Debug, Clone)]
pub enum StatusBadge {
    Amend,
    ReviewVerdict(ReviewVerdict),
    ReviewGate(ReviewGateState),
    FeedbackStatus(FeedbackFileStatus),
    State(String),
    Warning,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NeedsAttention {
    None,
    AwaitingPlanReview,
    AwaitingImplReview,
    StaleFeedback,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowPresence {
    NotRendered,
    Rendered,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowProjection {
    Insert,
    Replace,
    Remove,
    Noop,
}

pub fn project_row(prior: RowPresence, next: RowPresence) -> RowProjection {
    match (prior, next) {
        (RowPresence::NotRendered, RowPresence::Rendered) => RowProjection::Insert,
        (RowPresence::Rendered, RowPresence::Rendered) => RowProjection::Replace,
        (RowPresence::Rendered, RowPresence::NotRendered) => RowProjection::Remove,
        (RowPresence::NotRendered, RowPresence::NotRendered) => RowProjection::Noop,
    }
}

#[derive(Debug, Clone)]
pub struct TimelineAction {
    pub label: &'static str,
    pub href: String,
    pub icon: ActionIcon,
    pub aria_label: String,
}

#[derive(Debug, Clone, Copy)]
pub enum ActionIcon {
    Open,
    View,
    Diff,
    Previous,
    Sound,
    Delete,
    Check,
}

pub struct TimelineRenderCtx<'a> {
    pub session_id: &'a str,
    pub session_id_for_prefix: Option<&'a str>,
    pub session_title_for_prefix: Option<&'a str>,
    pub plan_rev_number_by_id: &'a HashMap<i64, i64>,
    pub plan_preview_by_id: &'a HashMap<i64, String>,
    pub amend_by_sha: &'a HashMap<String, crate::daemon::amend::AmendInfo>,
    pub commit_preview_by_sha: &'a HashMap<String, String>,
    pub feedback_target_by_id: &'a HashMap<i64, (String, String)>,
    pub feedback_preview_by_id: &'a HashMap<i64, String>,
    pub feedback_kind_by_id: &'a HashMap<i64, FeedbackKind>,
    pub feedback_author_by_id: &'a HashMap<i64, String>,
    pub feedback_status_by_author_kind: &'a HashMap<(FeedbackKind, String), FeedbackFileStatus>,
}

// ---------- Plan revision pages + diff types ----------

pub struct PlanRevisionView {
    pub session_id: String,
    pub rev: PlanRevision,
    pub prev: Option<PlanRevisionLink>,
    pub next: Option<PlanRevisionLink>,
    pub body_html: String,
    pub feedback: Vec<DiffViewFeedback>,
}

/// Minimal info needed to render a prev/next link on the plan-revision page.
pub struct PlanRevisionLink {
    pub rev_id: i64,
    pub revision_number: i64,
}

pub struct PlanRevisionDiffView {
    pub session_id: String,
    pub cur: PlanRevision,
    pub prev_rev_number: i64,
    pub lines: Vec<DiffLine>,
    pub body_html: String,
    pub feedback: Vec<DiffViewFeedback>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffLineKind {
    Context,
    Insert,
    Delete,
}

pub struct DiffLine {
    pub kind: DiffLineKind,
    pub content: String,
}

fn short(sha: &str) -> String {
    sha.chars().take(12).collect()
}

fn watched_artifacts_strip(d: &SessionDetail) -> Markup {
    let plan_file_name = Path::new(&d.session.plan_file_path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(&d.session.plan_file_path);
    let plan_review_status = feedback_summary_label(&d.plan_feedback_files);
    let impl_review_status = feedback_summary_label(&d.impl_feedback_files);
    html! {
        section.watched-artifacts {
            details {
                summary {
                    span { "Watching " code.mono { (plan_file_name) } }
                    span.summary-separator { "·" }
                    span { "plan reviews " (d.plan_feedback_files.len()) " (" (plan_review_status) ")" }
                    span.summary-separator { "·" }
                    span { "impl reviews " (d.impl_feedback_files.len()) " (" (impl_review_status) ")" }
                    span.summary-separator { "·" }
                    @if d.git_logs_head_path.is_none() {
                        span.artifact-chip.warn { "git log missing" }
                        span.summary-separator { "·" }
                    }
                    span { "paths ▾" }
                }
                dl {
                    dt { "Plan file" } dd.path { (d.session.plan_file_path) }
                    dt { "Git logs/HEAD" }
                    dd.path {
                        @match d.git_logs_head_path.as_deref() {
                            Some(p) => (p),
                            None => "(unresolved)",
                        }
                    }
                    dt { "Plan feedback directory" }
                    dd.path {
                        @match d.plan_feedback_dir_path.as_deref() {
                            Some(p) => (p),
                            None => "(not yet attached)",
                        }
                    }
                    dt { "Impl feedback directory" }
                    dd.path {
                        @match d.impl_feedback_dir_path.as_deref() {
                            Some(p) => (p),
                            None => "(not yet attached)",
                        }
                    }
                }
                (feedback_files_table("Plan feedback files", &d.plan_feedback_files))
                (feedback_files_table("Implementation feedback files", &d.impl_feedback_files))
            }
        }
    }
}

fn feedback_summary_label(files: &[FeedbackFileSnapshot]) -> String {
    if files.is_empty() {
        return "none".to_string();
    }
    let non_current = files
        .iter()
        .filter(|s| s.status != FeedbackFileStatus::Current)
        .count();
    if non_current == 0 {
        "current".to_string()
    } else {
        format!("{non_current} stale")
    }
}

fn feedback_files_table(heading: &str, files: &[FeedbackFileSnapshot]) -> Markup {
    html! {
        h3 { (heading) }
        @if files.is_empty() {
            p.empty { "None yet. Reviewers drop `<author>.md` into the directory above." }
        } @else {
            table.feedback-files {
                thead {
                    tr {
                        th { "Author" }
                        th { "Status" }
                        th { "Last ingested" }
                        th { "Last ingested target" }
                    }
                }
                tbody {
                    @for s in files {
                        tr {
                            td.mono { (s.row.author_label) }
                            td { (feedback_file_badge(s.status)) }
                            td.relative { (relative_time(s.row.last_ingested_at)) }
                            td.mono {
                                @match (s.row.last_ingested_target_kind.as_deref(), s.row.last_ingested_target_id.as_deref()) {
                                    (Some(k), Some(id)) if k == "implementation_commit" => (short(id)),
                                    (Some(_), Some(id)) => (id),
                                    _ => "—",
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn feedback_file_badge(status: FeedbackFileStatus) -> Markup {
    let s = status.as_str();
    let class = format!("badge feedback-file-status {s}");
    html! { span.(class) { (s) } }
}

pub fn session_detail(d: &SessionDetail) -> Markup {
    let session_id = d.session.id.clone();
    layout(
        &format!("Trinity — {}", session_title(&d.session)),
        html! {
            (timeline_head_scripts())
            header.session-head {
                div.session-head-left {
                    a.back href="/" { "← all sessions" }
                    h1.session-id { (session_id) }
                    (active_plan_badge(d.active_plan.as_ref().map(|p| p.state.as_str())))
                    @if d.is_repo_effective {
                        span.kind-badge { "in effect" }
                    } @else if d.active_plan.is_some() {
                        form method="post" action={ "/sessions/" (session_id) "/claim" } class="inline-form" {
                            button type="submit" class="primary" {
                                "Set in effect"
                            }
                        }
                    }
                    @if d.archived_count > 0 {
                        a.muted-link href={ "/sessions/" (session_id) "/history" } { "history (" (d.archived_count) ")" }
                    }
                }
                div.session-head-right {
                    @if let Some(plan) = &d.active_plan {
                        span.head-meta { "base " code.mono { (short(&plan.base_commit)) } }
                    }
                    span.head-meta { "plan: " span.path.mono { (d.session.plan_file_path) } }
                    (sound_test_button())
                }
            }

            (watched_artifacts_strip(d))
            (review_gate_strip(d))
            (active_plan_preview(d.active_plan_preview.as_ref()))

            section.timeline-section {
                div.section-title-row {
                    h2 { "Timeline" }
                }
                @if d.timeline_rows.is_empty() {
                    p.empty { "Nothing has happened yet." }
                }
                div.timeline-wrap {
                    section id="timeline-feed"
                            class="timeline-feed"
                            data-timeline-feed=""
                            hx-ext="sse"
                            sse-connect={ "/sessions/" (session_id) "/events?since=" (d.max_event_id) }
                            sse-swap="message"
                            hx-swap="none" {
                        @for row in &d.timeline_rows {
                            (timeline_row_article(row, false))
                        }
                    }
                }
            }
        },
    )
}

fn review_gate_strip(d: &SessionDetail) -> Markup {
    if d.plan_review_gate.is_none() && d.impl_review_gate.is_none() {
        return html! {};
    }
    html! {
        section.review-gates {
            @if let Some(gate) = &d.plan_review_gate {
                (review_gate_panel(&d.session.id, gate))
            }
            @if let Some(gate) = &d.impl_review_gate {
                (review_gate_panel(&d.session.id, gate))
            }
        }
    }
}

fn review_gate_panel(session_id: &str, gate: &ReviewGateDecision) -> Markup {
    let phase = gate.phase.as_str();
    let request_changes = join_agents(&gate.request_changes);
    let missing_approvals = join_agents(&gate.missing_approvals);
    let unmarked = join_agents(&gate.unmarked);
    html! {
        div.review-gate-card {
            div.review-gate-main {
                span.review-gate-phase { (phase) " review" }
                (review_gate_badge(gate.state))
                @if let Some(override_status) = &gate.override_status {
                    span.review-gate-override {
                        "overridden by " (override_status.actor)
                    }
                }
            }
            div.review-gate-detail {
                "approvals " (gate.approvals.len()) "/" (gate.participants.len())
                @if !gate.request_changes.is_empty() {
                    " · changes from " (request_changes)
                }
                @if !gate.missing_approvals.is_empty() {
                    " · waiting on " (missing_approvals)
                }
                @if !gate.unmarked.is_empty() {
                    " · unmarked " (unmarked)
                }
            }
            form method="post" action={ "/sessions/" (session_id) "/review_gate_override" } class="review-gate-actions" {
                input type="hidden" name="phase" value=(phase);
                button type="submit" name="state" value="ready" class="primary" {
                    "Mark ready"
                }
                button type="submit" name="state" value="changes_requested" class="danger" {
                    "Request changes"
                }
            }
        }
    }
}

fn join_agents(labels: &[crate::lifecycle::AgentLabel]) -> String {
    labels
        .iter()
        .map(|label| label.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

pub fn timeline_row_from_event(ev: &Event, ctx: &TimelineRenderCtx<'_>) -> TimelineRow {
    use crate::daemon::amend::AmendInfo;

    let payload: serde_json::Value =
        serde_json::from_str(&ev.payload).unwrap_or(serde_json::Value::Null);
    let mut status_badges = Vec::new();
    let mut needs_attention = NeedsAttention::None;
    let mut actions = Vec::new();
    let mut preview = None;

    let title = match ev.kind.as_str() {
        "plan_revision_created" => {
            let rev_id = ev.target_id.as_deref().and_then(|s| s.parse::<i64>().ok());
            let revision_number = rev_id
                .and_then(|id| ctx.plan_rev_number_by_id.get(&id).copied())
                .unwrap_or(0);
            if let Some(rev_id) = rev_id {
                preview = ctx.plan_preview_by_id.get(&rev_id).cloned();
                actions.push(TimelineAction {
                    label: "View",
                    href: format!("/sessions/{}/plan_revisions/{rev_id}", ctx.session_id),
                    icon: ActionIcon::View,
                    aria_label: format!("View plan revision #{revision_number}"),
                });
                if revision_number > 1 {
                    actions.push(TimelineAction {
                        label: "Diff",
                        href: format!("/sessions/{}/plan_revisions/{rev_id}/diff", ctx.session_id),
                        icon: ActionIcon::Diff,
                        aria_label: format!("Diff to #{}", revision_number - 1),
                    });
                }
            }
            TimelineTitle::PlanRevision { revision_number }
        }
        "impl_revision_created" => {
            let sha = ev.target_id.as_deref().unwrap_or("");
            preview = ctx.commit_preview_by_sha.get(sha).cloned();
            match ctx.amend_by_sha.get(sha) {
                Some(AmendInfo::Amend {
                    prev_sha,
                    amend_base_parent_sha,
                }) => {
                    status_badges.push(StatusBadge::Amend);
                    actions.push(TimelineAction {
                        label: "Prev rev",
                        href: format!("/sessions/{}/commits/{sha}?vs={prev_sha}", ctx.session_id),
                        icon: ActionIcon::Previous,
                        aria_label: "Diff vs previous amend".to_string(),
                    });
                    if let Some(base) = amend_base_parent_sha.as_deref() {
                        actions.push(TimelineAction {
                            label: "Full diff",
                            href: format!("/sessions/{}/commits/{sha}?vs={base}", ctx.session_id),
                            icon: ActionIcon::Diff,
                            aria_label: "Full diff since parent".to_string(),
                        });
                    } else {
                        actions.push(TimelineAction {
                            label: "Diff",
                            href: format!("/sessions/{}/commits/{sha}", ctx.session_id),
                            icon: ActionIcon::Diff,
                            aria_label: "Diff parent..commit".to_string(),
                        });
                    }
                }
                _ => actions.push(TimelineAction {
                    label: "Diff",
                    href: format!("/sessions/{}/commits/{sha}", ctx.session_id),
                    icon: ActionIcon::Diff,
                    aria_label: "Diff parent..commit".to_string(),
                }),
            }
            needs_attention = NeedsAttention::AwaitingImplReview;
            TimelineTitle::Commit {
                short_sha: short(sha),
            }
        }
        "head_reset_to_known_sha" => {
            let sha = ev.target_id.as_deref().unwrap_or("");
            actions.push(TimelineAction {
                label: "Diff",
                href: format!("/sessions/{}/commits/{sha}", ctx.session_id),
                icon: ActionIcon::Diff,
                aria_label: "Diff parent..commit".to_string(),
            });
            TimelineTitle::HeadReset {
                short_sha: short(sha),
            }
        }
        "feedback_added" | "feedback_updated" => {
            let fid = payload.get("feedback_id").and_then(|v| v.as_i64());
            if let Some(fid) = fid {
                preview = ctx.feedback_preview_by_id.get(&fid).cloned();
                let verdict = preview
                    .as_deref()
                    .map(crate::review_state::parse_verdict)
                    .unwrap_or(ReviewVerdict::Unmarked);
                status_badges.push(StatusBadge::ReviewVerdict(verdict));
                if let Some((target_kind, target_id)) = ctx.feedback_target_by_id.get(&fid) {
                    if target_kind == "plan_revision" {
                        let rev_id = target_id.parse::<i64>().unwrap_or(0);
                        let n = ctx.plan_rev_number_by_id.get(&rev_id).copied().unwrap_or(0);
                        actions.push(TimelineAction {
                            label: "Open",
                            href: format!(
                                "/sessions/{}/plan_revisions/{rev_id}#feedback-{fid}",
                                ctx.session_id
                            ),
                            icon: ActionIcon::Open,
                            aria_label: format!("Open in plan revision #{n}"),
                        });
                    } else {
                        actions.push(TimelineAction {
                            label: "Open",
                            href: format!(
                                "/sessions/{}/commits/{target_id}#feedback-{fid}",
                                ctx.session_id
                            ),
                            icon: ActionIcon::Open,
                            aria_label: format!("Open in commit {}", short(target_id)),
                        });
                    }
                }
            }
            let kind = fid
                .and_then(|id| ctx.feedback_kind_by_id.get(&id).copied())
                .or_else(|| {
                    ev.target_kind
                        .as_deref()
                        .and_then(feedback_kind_from_target_kind)
                })
                .unwrap_or(FeedbackKind::Plan);
            let author = fid
                .and_then(|id| ctx.feedback_author_by_id.get(&id).cloned())
                .unwrap_or_else(|| display_actor(&ev.actor));
            if let Some(status) = ctx
                .feedback_status_by_author_kind
                .get(&(kind, author.clone()))
                .copied()
            {
                if status == FeedbackFileStatus::Current {
                    status_badges.push(StatusBadge::FeedbackStatus(status));
                } else {
                    needs_attention = NeedsAttention::StaleFeedback;
                }
            }
            TimelineTitle::Feedback { kind, author }
        }
        "review_gate_changed" => {
            let phase = payload
                .get("phase")
                .and_then(|v| v.as_str())
                .and_then(ReviewPhase::parse)
                .unwrap_or(ReviewPhase::Plan);
            let to = payload
                .get("to")
                .and_then(|v| v.as_str())
                .and_then(ReviewGateState::parse)
                .unwrap_or(ReviewGateState::NeedsReview);
            let from = payload.get("from").and_then(|v| v.as_str()).unwrap_or("");
            status_badges.push(StatusBadge::ReviewGate(to));
            preview = Some(format!("{from} -> {}", to.as_str()));
            match (phase, to) {
                (ReviewPhase::Plan, ReviewGateState::ChangesRequested) => {
                    needs_attention = NeedsAttention::AwaitingPlanReview;
                }
                (ReviewPhase::Impl, ReviewGateState::ChangesRequested) => {
                    needs_attention = NeedsAttention::AwaitingImplReview;
                }
                _ => {}
            }
            TimelineTitle::ReviewGate { phase }
        }
        "state_transition" => {
            let from = payload.get("from").and_then(|v| v.as_str()).unwrap_or("");
            let to = payload.get("to").and_then(|v| v.as_str()).unwrap_or("");
            if !to.is_empty() {
                status_badges.push(StatusBadge::State(to.to_string()));
            }
            preview = match (from.is_empty(), to.is_empty()) {
                (false, false) => Some(format!("{from} -> {to}")),
                (true, false) => Some(to.to_string()),
                _ => None,
            };
            TimelineTitle::StateTransition
        }
        "session_claimed" => {
            preview = payload
                .get("previous_session_id")
                .and_then(|v| v.as_str())
                .map(|previous| format!("previously {previous}"));
            TimelineTitle::SessionClaimed
        }
        "agent_joined" => {
            let actor = payload
                .get("label")
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .unwrap_or_else(|| display_actor(&ev.actor));
            TimelineTitle::AgentJoined { actor }
        }
        "dirty_worktree_warning" => {
            status_badges.push(StatusBadge::Warning);
            preview = payload
                .get("porcelain")
                .and_then(|v| v.as_str())
                .and_then(first_line_preview);
            needs_attention = NeedsAttention::AwaitingImplReview;
            TimelineTitle::Warning {
                kind: WarningKind::DirtyWorktree,
            }
        }
        "plan_file_missing" => {
            status_badges.push(StatusBadge::Warning);
            preview = payload
                .get("path")
                .and_then(|v| v.as_str())
                .and_then(first_line_preview);
            needs_attention = NeedsAttention::AwaitingPlanReview;
            TimelineTitle::Warning {
                kind: WarningKind::PlanFileMissing,
            }
        }
        other => TimelineTitle::Other {
            kind: other.to_string(),
        },
    };

    TimelineRow {
        event_id: ev.id,
        event_kind: ev.kind.clone(),
        session_id_for_prefix: ctx.session_id_for_prefix.map(str::to_string),
        session_title_for_prefix: ctx.session_title_for_prefix.map(str::to_string),
        actor: display_actor(&ev.actor),
        ts: ev.ts,
        title,
        preview,
        actions,
        status_badges,
        needs_attention,
        kind_class: timeline_kind_class(&ev.kind),
    }
}

pub fn timeline_row_article(row: &TimelineRow, live: bool) -> Markup {
    let mut class_attr = format!("entry {}", row.kind_class);
    if live {
        class_attr.push_str(" live");
    }
    if row.needs_attention != NeedsAttention::None {
        class_attr.push_str(" needs-attention");
    }
    html! {
        article id={ "event-" (row.event_id) }
                class=(class_attr)
                data-event-kind=(row.event_kind) {
            header.entry-top {
                div.entry-title-group {
                    @if let Some(sid) = &row.session_id_for_prefix {
                        div.entry-session-prefix {
                            a href={ "/sessions/" (sid) } {
                                @if let Some(title) = &row.session_title_for_prefix {
                                    (title)
                                } @else {
                                    (sid)
                                }
                            }
                        }
                    }
                    div.entry-title {
                        (timeline_title_markup(&row.title))
                        @for badge in &row.status_badges {
                            (status_badge_markup(badge))
                        }
                        (needs_attention_marker(row.needs_attention))
                    }
                }
                @if !row.actions.is_empty() {
                    nav.entry-actions aria-label="Timeline actions" {
                        @for action in &row.actions {
                            (timeline_action_link(action))
                        }
                    }
                }
            }
            div.entry-meta {
                span.actor { (row.actor) }
                " · " (relative_time_tag(Some(row.ts)))
            }
            @if let Some(p) = &row.preview {
                div.entry-preview { (p) }
            }
        }
    }
}

pub fn timeline_row_oob(row: &TimelineRow, target_id: &str) -> Markup {
    html! {
        template hx-swap-oob={ "afterbegin:#" (target_id) } {
            (timeline_row_article(row, true))
        }
    }
}

fn timeline_title_markup(title: &TimelineTitle) -> Markup {
    match title {
        TimelineTitle::PlanRevision { revision_number } => html! {
            "Plan revision "
            @if *revision_number > 0 { "#" (revision_number) } @else { "—" }
        },
        TimelineTitle::Commit { short_sha } => html! { "Commit " code.mono { (short_sha) } },
        TimelineTitle::HeadReset { short_sha } => {
            html! { "HEAD reset to " code.mono { (short_sha) } }
        }
        TimelineTitle::Feedback { kind, author } => {
            html! { (kind.as_str()) " feedback from " span.actor { (author) } }
        }
        TimelineTitle::ReviewGate { phase } => html! { (phase.as_str()) " review gate" },
        TimelineTitle::StateTransition => html! { "State changed" },
        TimelineTitle::SessionClaimed => html! { "Set in effect" },
        TimelineTitle::AgentJoined { actor } => html! { (actor) " joined" },
        TimelineTitle::Warning { kind } => match kind {
            WarningKind::DirtyWorktree => html! { "Dirty worktree" },
            WarningKind::PlanFileMissing => html! { "Plan file missing" },
        },
        TimelineTitle::Other { kind } => html! { (kind) },
    }
}

fn status_badge_markup(badge: &StatusBadge) -> Markup {
    match badge {
        StatusBadge::Amend => html! { span.kind-badge.amend { "amend" } },
        StatusBadge::ReviewVerdict(verdict) => feedback_verdict_badge(*verdict),
        StatusBadge::ReviewGate(state) => review_gate_badge(*state),
        StatusBadge::FeedbackStatus(status) => feedback_file_badge(*status),
        StatusBadge::State(state) => html! { span.kind-badge { (state) } },
        StatusBadge::Warning => html! { span.kind-badge.warning-badge { "warning" } },
    }
}

fn review_gate_badge(state: ReviewGateState) -> Markup {
    let class = format!("kind-badge review-gate {}", state.as_str());
    let label = match state {
        ReviewGateState::NeedsReview => "needs review",
        ReviewGateState::ChangesRequested => "changes requested",
        ReviewGateState::Ready => "ready",
    };
    html! { span.(class) { (label) } }
}

fn feedback_verdict_badge(verdict: ReviewVerdict) -> Markup {
    let class = format!("kind-badge verdict {}", verdict.css_class());
    let label = match verdict {
        ReviewVerdict::Approve => "✓ approve",
        ReviewVerdict::RequestChanges => "! changes",
        ReviewVerdict::Unmarked => "? unmarked",
    };
    html! { span.(class) title=(verdict.marker()) { (label) } }
}

fn needs_attention_marker(needs: NeedsAttention) -> Markup {
    let label = match needs {
        NeedsAttention::None => return html! {},
        NeedsAttention::AwaitingPlanReview => "needs plan review",
        NeedsAttention::AwaitingImplReview => "needs impl review",
        NeedsAttention::StaleFeedback => "feedback stale",
    };
    html! { span.attention-badge { (label) } }
}

fn timeline_action_link(action: &TimelineAction) -> Markup {
    html! {
        a.icon-action href=(action.href) title=(action.aria_label) aria-label=(action.aria_label) {
            (action_icon(action.icon))
            span.action-label { (action.label) }
        }
    }
}

fn action_icon(icon: ActionIcon) -> Markup {
    let svg = match icon {
        ActionIcon::Open => include_str!("icons/external-link.svg"),
        ActionIcon::View => include_str!("icons/file-text.svg"),
        ActionIcon::Diff => include_str!("icons/git-compare-arrows.svg"),
        ActionIcon::Previous => include_str!("icons/history.svg"),
        ActionIcon::Sound => include_str!("icons/volume-2.svg"),
        ActionIcon::Delete => include_str!("icons/trash-2.svg"),
        ActionIcon::Check => include_str!("icons/check-circle-2.svg"),
    };
    html! { span.action-icon aria-hidden="true" { (PreEscaped(svg)) } }
}

fn active_plan_preview(preview: Option<&ActivePlanPreview>) -> Markup {
    let Some(preview) = preview else {
        return html! {};
    };
    let summary = html! {
        span { "Active plan revision #" (preview.revision_number) }
        span.plan-preview-meta {
            (relative_time_tag(Some(preview.created_at)))
            " · " code.mono { "rev " (preview.rev_id) }
        }
    };
    if preview.is_long {
        html! {
            section.active-plan-preview {
                details.active-plan {
                    summary { (summary) }
                    article.markdown { (PreEscaped(&preview.body_html)) }
                }
            }
        }
    } else {
        html! {
            section.active-plan-preview {
                header.active-plan-head { (summary) }
                article.markdown { (PreEscaped(&preview.body_html)) }
            }
        }
    }
}

fn sound_test_button() -> Markup {
    html! {
        button.icon-action type="button" title="Test notification sound" aria-label="Test notification sound" data-sound-test="" {
            (action_icon(ActionIcon::Sound))
            span { "Test sound" }
        }
    }
}

fn relative_time_tag(ts: Option<i64>) -> Markup {
    match ts {
        Some(ts) => html! {
            time.relative datetime=(absolute_time(ts)) title=(absolute_time(ts)) data-ts=(ts) {
                (relative_time(Some(ts)))
            }
        },
        None => html! { span.relative { "—" } },
    }
}

fn timeline_kind_class(kind: &str) -> &'static str {
    match kind {
        "plan_revision_created" => "plan-rev",
        "impl_revision_created" => "impl-commit",
        "head_reset_to_known_sha" => "head-reset",
        "feedback_added" | "feedback_updated" => "feedback",
        "review_gate_changed" => "review-gate",
        "session_claimed" => "state",
        "state_transition" => "state",
        "agent_joined" => "meta-event",
        "dirty_worktree_warning" | "plan_file_missing" => "warning",
        _ => "meta-event",
    }
}

fn feedback_kind_from_target_kind(kind: &str) -> Option<FeedbackKind> {
    match kind {
        "plan_revision" => Some(FeedbackKind::Plan),
        "implementation_commit" => Some(FeedbackKind::Impl),
        _ => None,
    }
}

fn display_actor(actor: &str) -> String {
    actor
        .strip_prefix("agent:")
        .or_else(|| actor.strip_prefix("system:"))
        .unwrap_or(actor)
        .to_string()
}

fn first_line_preview(body: &str) -> Option<String> {
    body.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(|line| truncate_chars(line, 180))
}

fn truncate_chars(value: &str, max: usize) -> String {
    let mut out = String::new();
    for (idx, ch) in value.chars().enumerate() {
        if idx >= max {
            out.push('…');
            return out;
        }
        out.push(ch);
    }
    out
}

fn timeline_head_scripts() -> Markup {
    html! {
        script src="https://unpkg.com/htmx.org@2.0.3" defer {}
        script src="https://unpkg.com/htmx-ext-sse@2.2.2" defer {}
        script { (PreEscaped(TIMELINE_FLIP_JS)) }
    }
}

/// FLIP-style shift-down: when an SSE OOB row prepends to `#timeline-feed`,
/// the existing rows would otherwise reflow instantly. We snapshot the
/// existing rows' positions before the swap, then on `htmx:oobAfterSwap`
/// compute the delta and play it backwards as a transition so the rows
/// appear to shift down to make room for the new entry. Falls back to a
/// no-op when the user prefers reduced motion.
const TIMELINE_FLIP_JS: &str = r#"
(() => {
  const FEED_SEL = '[data-timeline-feed]';
  const REDUCED = window.matchMedia('(prefers-reduced-motion: reduce)').matches;
  let snapshot = new Map();
  let audioCtx = null;

  function relative(ts) {
    if (!ts || ts <= 0) return 'stale';
    const diff = Math.floor(Date.now() / 1000) - ts;
    if (diff < 0) return 'in the future';
    if (diff < 60) return 'just now';
    if (diff < 3600) return `${Math.floor(diff / 60)}m ago`;
    if (diff < 86400) return `${Math.floor(diff / 3600)}h ago`;
    if (diff < 604800) return `${Math.floor(diff / 86400)}d ago`;
    return new Date(ts * 1000).toLocaleDateString();
  }

  function updateTimes() {
    for (const el of document.querySelectorAll('time.relative[data-ts]')) {
      const ts = Number(el.dataset.ts);
      el.textContent = relative(ts);
    }
  }

  function ping() {
    try {
      const Ctor = window.AudioContext || window.webkitAudioContext;
      if (!Ctor) return;
      audioCtx = audioCtx || new Ctor();
      const playTone = () => {
        const now = audioCtx.currentTime;
        const master = audioCtx.createGain();
        master.gain.setValueAtTime(0.0001, now);
        master.gain.exponentialRampToValueAtTime(0.16, now + 0.018);
        master.gain.exponentialRampToValueAtTime(0.0001, now + 1.05);
        master.connect(audioCtx.destination);

        for (const [offset, base, level] of [[0, 1174.66, 0.46], [0.09, 1567.98, 0.34], [0.19, 2349.32, 0.22]]) {
          for (const [ratio, mix] of [[1, 1], [2.01, 0.22], [3.98, 0.08]]) {
            const osc = audioCtx.createOscillator();
            const gain = audioCtx.createGain();
            const start = now + offset;
            osc.type = 'sine';
            osc.frequency.setValueAtTime(base * ratio, start);
            osc.frequency.exponentialRampToValueAtTime(base * ratio * 1.006, start + 0.2);
            gain.gain.setValueAtTime(0.0001, start);
            gain.gain.exponentialRampToValueAtTime(level * mix, start + 0.012);
            gain.gain.exponentialRampToValueAtTime(0.0001, start + 0.72);
            osc.connect(gain).connect(master);
            osc.start(start);
            osc.stop(start + 0.78);
          }
        }
      };
      if (audioCtx.state === 'suspended') {
        audioCtx.resume().then(playTone).catch(() => {});
      } else {
        playTone();
      }
    } catch (_) {}
  }

  function snap() {
    if (REDUCED) return;
    snapshot.clear();
    for (const feed of document.querySelectorAll(FEED_SEL)) {
      for (const el of feed.children) {
        if (!el.id) continue;
        snapshot.set(el.id, el.getBoundingClientRect().top);
      }
    }
  }
  function play() {
    updateTimes();
    const live = Array.from(document.querySelectorAll('article.entry.live'));
    if (live.length) {
      ping();
      setTimeout(() => live.forEach((el) => el.classList.remove('live')), 1600);
    }
    if (REDUCED) return;
    for (const feed of document.querySelectorAll(FEED_SEL)) {
      const clip = feed.parentElement ? feed.parentElement.getBoundingClientRect() : null;
      for (const el of feed.children) {
        if (!el.id || !snapshot.has(el.id)) continue;
        const rect = el.getBoundingClientRect();
        if (clip && (rect.bottom < clip.top || rect.top > clip.bottom)) continue;
        const prev = snapshot.get(el.id);
        const next = rect.top;
        const delta = prev - next;
        if (Math.abs(delta) < 0.5) continue;
        el.style.transition = 'none';
        el.style.transform = `translateY(${delta}px)`;
        requestAnimationFrame(() => {
          el.style.transition = 'transform 220ms ease-out';
          el.style.transform = '';
          el.addEventListener('transitionend', () => {
            el.style.transition = '';
          }, { once: true });
        });
      }
    }
    snapshot.clear();
  }
  document.addEventListener('click', (event) => {
    if (event.target.closest('[data-sound-test]')) ping();
  });
  updateTimes();
  setInterval(updateTimes, 30000);
  document.addEventListener('htmx:beforeSwap', snap, true);
  document.addEventListener('htmx:oobBeforeSwap', snap, true);
  document.addEventListener('htmx:afterSwap', play, true);
  document.addEventListener('htmx:oobAfterSwap', play, true);
})();
"#;

// ---------- History ----------

pub fn session_history(session: &Session, archived: &[Plan]) -> Markup {
    layout(
        &format!("Trinity — {} history", session.id),
        html! {
            nav.crumbs {
                a href="/" { "← all sessions" } " · "
                a href={ "/sessions/" (session.id) } { "back to session" }
            }
            h1 { "History: " (session.id) }
            @if archived.is_empty() {
                p.empty { "No archived plans in this session yet." }
            }
            @if !archived.is_empty() {
                table.history {
                    thead {
                        tr {
                            th { "plan_id" }
                            th { "Base commit" }
                            th { "Archived" }
                            th { "Started" }
                            th {}
                        }
                    }
                    tbody {
                        @for p in archived {
                            tr {
                                td.mono { (p.id) }
                                td.mono { (short(&p.base_commit)) }
                                td.relative { (relative_time(p.archived_at)) }
                                td.relative { (relative_time(Some(p.started_at))) }
                                td { a href={ "/sessions/" (session.id) "/history/" (p.id) } { "view" } }
                            }
                        }
                    }
                }
            }
        },
    )
}

pub fn history_plan_detail(
    session: &Session,
    plan: &Plan,
    revisions: &[PlanRevision],
    latest_body_html: &str,
    impl_revisions: &[ImplementationRevision],
    feedback: &[FeedbackItem],
) -> Markup {
    layout(
        &format!("Trinity — archived plan #{}", plan.id),
        html! {
            nav.crumbs {
                a href="/" { "← all sessions" } " · "
                a href={ "/sessions/" (session.id) } { "session" } " · "
                a href={ "/sessions/" (session.id) "/history" } { "history" }
            }
            h1 { "Archived plan #" (plan.id) " " span.badge.archived { "archived" } }
            section.meta {
                dl {
                    dt { "Base commit" }   dd.mono { (short(&plan.base_commit)) }
                    dt { "Started" }       dd.relative { (relative_time(Some(plan.started_at))) }
                    dt { "Archived" }      dd.relative { (relative_time(plan.archived_at)) }
                    dt { "Revisions" }     dd { (revisions.len()) }
                    dt { "Impl commits" }  dd { (impl_revisions.len()) }
                    dt { "Feedback rows" } dd { (feedback.len()) }
                }
            }
            section.plan-review {
                h2 { "Latest plan body" }
                article.markdown { (PreEscaped(latest_body_html.to_string())) }
            }
            @if !impl_revisions.is_empty() {
                section.impl-review {
                    h2 { "Implementation commits" }
                    ol {
                        @for rev in impl_revisions {
                            li {
                                a href={ "/sessions/" (session.id) "/commits/" (rev.commit_sha) } {
                                    code.mono { (short(&rev.commit_sha)) }
                                }
                                " · " (rev.commit_message.lines().next().unwrap_or(""))
                            }
                        }
                    }
                }
            }
            @if !feedback.is_empty() {
                section.plan-review {
                    h2 { "Feedback (historical)" }
                    @for item in feedback {
                        (feedback_card(item))
                    }
                }
            }
        },
    )
}

// ---------- Commit diff page ----------

pub fn commit_diff(view: &CommitDiffView) -> Markup {
    let short_sha = short(&view.commit.commit_sha);
    let base_chip = match &view.base_label {
        super::http::DiffBaseLabel::Parent(Some(p)) => format!("parent {}..", short(p)),
        super::http::DiffBaseLabel::Parent(None) => "root commit".to_string(),
        super::http::DiffBaseLabel::Override(s) => format!("from {}.. (base override)", short(s)),
    };
    let file_count = view.files.len();
    let changed_lines: usize = view.files.iter().map(FileDiff::changed_lines).sum();
    let file_index_class = if view.files.len() > 8 {
        "file-index two-col"
    } else {
        "file-index"
    };
    let review_count = view.feedback.len();
    layout_wide(
        &format!("Trinity — commit {}", short_sha),
        html! {
            (detail_header(&view.session_id, &format!("Commit {short_sha}"), html! {
                span.base-chip { (base_chip) }
                @if view.is_amend {
                    span.kind-badge.amend { "amend" }
                }
                @if review_count == 0 {
                    span.muted { "No reviews yet · 0" }
                } @else {
                    a.pill href="#reviews" { "Reviews · " (review_count) " ↓" }
                }
            }))
            section.detail-meta {
                "Full SHA: " code.mono { (view.commit.commit_sha) }
                " · " (relative_time(Some(view.commit.created_at)))
                " · " (file_count) " files"
                " · +" (view.files.iter().map(|f| f.additions).sum::<usize>())
                " -" (view.files.iter().map(|f| f.deletions).sum::<usize>())
                @if let Some(b) = &view.commit.branch { " · branch " code { (b) } }
                @if view.commit.is_head != 0 { " · " span.head-tag { "HEAD" } }
            }
            details.commit-msg-fold open {
                summary { "Commit message" }
                pre.commit-msg { (view.commit.commit_message) }
            }
            details.diff-stat-fold {
                summary { "Diff stat" }
                pre.diff-stat { (view.commit.diff_stat) }
            }
            (feedback_panel(&view.feedback))
            @if view.files.is_empty() {
                section.diff-pane.raw-diff { pre { (view.diff) } }
            } @else {
                nav.(file_index_class) aria-label="Files changed" {
                    @for (idx, file) in view.files.iter().enumerate() {
                        a href={ "#" (file.anchor(idx)) } {
                            span.file-index-path { (file.path) }
                            span.file-index-stat {
                                "+" (file.additions) " -" (file.deletions)
                            }
                        }
                    }
                }
                section.structured-diff aria-label="Commit diff" data-changed-lines=(changed_lines) {
                    @for (idx, file) in view.files.iter().enumerate() {
                        (render_file_diff(file, idx, view.files.len()))
                    }
                }
            }
        },
    )
}

fn render_file_diff(file: &FileDiff, idx: usize, total_files: usize) -> Markup {
    let anchor = file.anchor(idx);
    let collapsed = file.is_collapsed_by_default(total_files);
    let path_label = if matches!(file.mode, FileDiffMode::Renamed) {
        file.old_path
            .as_ref()
            .map(|old| format!("{old} -> {}", file.path))
            .unwrap_or_else(|| file.path.clone())
    } else {
        file.path.clone()
    };
    let summary = html! {
        span.file-diff-path { (path_label) }
        span.file-mode { (file_mode_label(&file.mode)) }
        span.file-stat { "+" (file.additions) " -" (file.deletions) }
    };
    if collapsed {
        html! {
            details.file-diff id=(anchor) {
                summary { (summary) }
                (render_file_diff_body(file))
            }
        }
    } else {
        html! {
            details.file-diff id=(anchor) open {
                summary { (summary) }
                (render_file_diff_body(file))
            }
        }
    }
}

fn render_file_diff_body(file: &FileDiff) -> Markup {
    if file.binary {
        return html! { div.binary-diff { "Binary file changed." } };
    }
    html! {
        div.diff-table role="table" {
            @for hunk in &file.hunks {
                div.diff-hunk-header role="row" {
                    span.lineno {}
                    span.lineno {}
                    span.diff-content { (hunk.header) }
                }
                @for line in &hunk.lines {
                    (render_parsed_diff_line(line))
                }
            }
        }
    }
}

fn render_parsed_diff_line(line: &crate::daemon::diff_parser::DiffLine) -> Markup {
    let cls = match line.kind {
        ParsedDiffLineKind::Insert => "diff-row ins",
        ParsedDiffLineKind::Delete => "diff-row del",
        ParsedDiffLineKind::Context => "diff-row ctx",
        ParsedDiffLineKind::Meta => "diff-row meta",
    };
    let marker = match line.kind {
        ParsedDiffLineKind::Insert => "+",
        ParsedDiffLineKind::Delete => "-",
        ParsedDiffLineKind::Context => " ",
        ParsedDiffLineKind::Meta => "\\",
    };
    html! {
        div.(cls) role="row" {
            span.lineno { @if let Some(n) = line.old_lineno { (n) } }
            span.lineno { @if let Some(n) = line.new_lineno { (n) } }
            span.diff-content {
                span.diff-marker { (marker) }
                (line.content)
            }
        }
    }
}

fn file_mode_label(mode: &FileDiffMode) -> &'static str {
    match mode {
        FileDiffMode::Added => "added",
        FileDiffMode::Removed => "removed",
        FileDiffMode::Renamed => "renamed",
        FileDiffMode::Modified => "modified",
    }
}

// ---------- helpers ----------

fn feedback_card(item: &FeedbackItem) -> Markup {
    let updated = item.updated_at != item.created_at;
    html! {
        div.feedback {
            header.feedback-head {
                span.actor { (item.author_label) }
                " · " (feedback_verdict_badge(item.verdict))
                " · " span.target { (item.target_label.clone().unwrap_or_else(|| "—".into())) }
                " · " span.relative { (relative_time(Some(item.created_at))) }
                @if updated {
                    " · " span.muted { "updated " (relative_time(Some(item.updated_at))) }
                }
            }
            div.feedback-body { pre { (item.body) } }
        }
    }
}

fn session_title(s: &Session) -> String {
    s.display_title.clone().unwrap_or_else(|| s.id.clone())
}

fn active_plan_badge(state: Option<&str>) -> Markup {
    match state {
        Some("planning") => html! { span.badge.planning { "planning" } },
        Some("implementing") => html! { span.badge.impl-review { "implementing" } },
        Some("archived") => html! { span.badge.archived { "archived" } },
        Some(other) => html! { span.badge { (other) } },
        None => html! { span.badge.archived { "no active plan" } },
    }
}

fn relative_time(ts: Option<i64>) -> String {
    let Some(ts) = ts else { return "—".into() };
    if ts <= 0 {
        return "stale".into();
    }
    let now = chrono::Utc::now().timestamp();
    let diff = now - ts;
    if diff < 0 {
        return "in the future".into();
    }
    if diff < 60 {
        return "just now".into();
    }
    if diff < 3600 {
        return format!("{}m ago", diff / 60);
    }
    if diff < 86400 {
        return format!("{}h ago", diff / 3600);
    }
    format!("{}d ago", diff / 86400)
}

// ---------- Plan revision pages ----------

pub fn plan_revision_view(view: &PlanRevisionView) -> Markup {
    let title = format!("Plan revision #{}", view.rev.revision_number);
    layout_wide(
        &format!("Trinity — {title}"),
        html! {
            (detail_header(&view.session_id, &title, html! {
                @if let Some(prev) = &view.prev {
                    a.pill href={ "/sessions/" (view.session_id) "/plan_revisions/" (view.rev.id) "/diff" } { "Diff to #" (prev.revision_number) }
                    a.pill href={ "/sessions/" (view.session_id) "/plan_revisions/" (prev.rev_id) } { "← #" (prev.revision_number) }
                }
                @if let Some(next) = &view.next {
                    a.pill href={ "/sessions/" (view.session_id) "/plan_revisions/" (next.rev_id) } { "#" (next.revision_number) " →" }
                }
            }))
            section.detail-meta {
                "Revision #" (view.rev.revision_number) " · " (relative_time(Some(view.rev.created_at))) " · "
                code.mono { (short(&view.rev.content_hash)) }
            }
            article.markdown.detail-body { (PreEscaped(&view.body_html)) }
            (feedback_panel(&view.feedback))
        },
    )
}

pub fn plan_revision_diff(view: &PlanRevisionDiffView) -> Markup {
    let title = format!("Plan revision #{}", view.cur.revision_number);
    let base_chip = format!("from #{}", view.prev_rev_number);
    layout_wide(
        &format!("Trinity — {title} diff"),
        html! {
            (detail_header(&view.session_id, &title, html! {
                span.base-chip { (base_chip) }
                a.pill href={ "/sessions/" (view.session_id) "/plan_revisions/" (view.cur.id) } { "View body" }
            }))
            section.diff-pane {
                @for line in &view.lines {
                    @let cls = match line.kind {
                        DiffLineKind::Insert  => "ins",
                        DiffLineKind::Delete  => "del",
                        DiffLineKind::Context => "ctx",
                    };
                    div.diff-line.(cls) { (line.content) }
                }
            }
            details.detail-body-fold {
                summary { "Show full revision body" }
                article.markdown.detail-body { (PreEscaped(&view.body_html)) }
            }
            (feedback_panel(&view.feedback))
        },
    )
}

/// Render the inline feedback panel used at the bottom of every artifact
/// detail page. Each block carries `id="feedback-<id>"` so the timeline's
/// `Open in …` link lands at the right anchor.
fn feedback_panel(feedback: &[DiffViewFeedback]) -> Markup {
    if feedback.is_empty() {
        return html! {};
    }
    html! {
        section.inline-feedback id="reviews" {
            h2 { "Feedback" }
            @for fb in feedback {
                article.inline-feedback-item id={ "feedback-" (fb.feedback_id) } {
                    header.feedback-head {
                        span.actor { (fb.author_label) }
                        " · " span.kind-badge { (fb.feedback_kind) }
                        " · " (feedback_verdict_badge(fb.verdict))
                        @if let Some(status) = fb.file_status {
                            " · " (feedback_file_badge(status))
                        }
                        " · " span.relative title=(absolute_time(fb.created_at)) { (relative_time(Some(fb.created_at))) }
                        " · " a.anchor href={ "#feedback-" (fb.feedback_id) } { "#" (fb.feedback_id) }
                    }
                    div.feedback-body.markdown { (PreEscaped(&fb.body_html)) }
                    @if let Some(path) = &fb.file_path {
                        footer.feedback-file-path { "file: " span.path.mono { (path) } }
                    }
                }
            }
        }
    }
}

fn absolute_time(ts: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp(ts, 0)
        .map(|d| d.to_rfc3339())
        .unwrap_or_default()
}

fn detail_header(session_id: &str, title: &str, actions: Markup) -> Markup {
    html! {
        header.detail-header {
            div.detail-crumbs {
                a href="/" { "← all sessions" } " · "
                a href={ "/sessions/" (session_id) } { (session_id) }
            }
            div.detail-title { (title) }
            div.detail-actions { (actions) }
        }
    }
}

/// Render one SSE OOB fragment for a single `events` row. Used by the
/// SSE handler to push live timeline insertions. Builds a per-event
/// `TimelineRenderCtx` from focused DB lookups so the live row carries
/// the same action pills as the initial-page render.
pub async fn sse_event_fragment(
    pool: &sqlx::SqlitePool,
    session_id: &crate::lifecycle::SessionId,
    ev: &Event,
) -> Markup {
    use crate::domain::FeedbackTargetRef;
    use crate::storage::{feedback as feedback_store, implementation_revisions, plan_revisions};

    let mut plan_rev_number_by_id: HashMap<i64, i64> = HashMap::new();
    let mut plan_preview_by_id: HashMap<i64, String> = HashMap::new();
    let mut amend_by_sha: HashMap<String, crate::daemon::amend::AmendInfo> = HashMap::new();
    let mut commit_preview_by_sha: HashMap<String, String> = HashMap::new();
    let mut feedback_target_by_id: HashMap<i64, (String, String)> = HashMap::new();
    let mut feedback_preview_by_id: HashMap<i64, String> = HashMap::new();
    let mut feedback_kind_by_id: HashMap<i64, FeedbackKind> = HashMap::new();
    let mut feedback_author_by_id: HashMap<i64, String> = HashMap::new();
    let feedback_status_by_author_kind: HashMap<(FeedbackKind, String), FeedbackFileStatus> =
        HashMap::new();

    match ev.kind.as_str() {
        "plan_revision_created" => {
            if let Some(rev_id) = ev.target_id.as_deref().and_then(|s| s.parse::<i64>().ok())
                && let Ok(Some(rev)) = plan_revisions::fetch(pool, rev_id).await
            {
                plan_rev_number_by_id.insert(rev.id, rev.revision_number);
                if let Some(preview) = first_line_preview(&rev.body) {
                    plan_preview_by_id.insert(rev.id, preview);
                }
            }
        }
        "impl_revision_created" | "head_reset_to_known_sha" => {
            if let Some(plan_id) = ev.plan_id
                && let Ok(rows) = implementation_revisions::list_for_plan(pool, plan_id).await
            {
                let infos = crate::daemon::amend::classify(&rows);
                for (row, info) in rows.iter().zip(infos) {
                    amend_by_sha.insert(row.commit_sha.clone(), info);
                    if let Some(preview) = first_line_preview(&row.commit_message) {
                        commit_preview_by_sha.insert(row.commit_sha.clone(), preview);
                    }
                }
            }
        }
        "feedback_added" | "feedback_updated" => {
            let payload: serde_json::Value =
                serde_json::from_str(&ev.payload).unwrap_or(serde_json::Value::Null);
            if let Some(fid) = payload.get("feedback_id").and_then(|v| v.as_i64())
                && let Ok(Some(rec)) = feedback_store::fetch(pool, fid).await
            {
                if let Some(preview) = first_line_preview(&rec.body) {
                    feedback_preview_by_id.insert(rec.id, preview);
                }
                feedback_author_by_id.insert(rec.id, rec.author_label.as_str().to_string());
                let (kind, target) = match &rec.target {
                    FeedbackTargetRef::PlanRevision(rev_id) => {
                        if let Ok(Some(rev)) = plan_revisions::fetch(pool, *rev_id).await {
                            plan_rev_number_by_id.insert(rev.id, rev.revision_number);
                        }
                        ("plan_revision".to_string(), rev_id.to_string())
                    }
                    FeedbackTargetRef::ImplementationCommit(sha) => (
                        "implementation_commit".to_string(),
                        sha.as_str().to_string(),
                    ),
                };
                let feedback_kind = match rec.target.kind() {
                    crate::domain::TargetKind::PlanRevision => FeedbackKind::Plan,
                    crate::domain::TargetKind::ImplementationCommit => FeedbackKind::Impl,
                };
                feedback_kind_by_id.insert(rec.id, feedback_kind);
                feedback_target_by_id.insert(rec.id, (kind, target));
            }
        }
        _ => {}
    }

    let ctx = TimelineRenderCtx {
        session_id: session_id.as_str(),
        session_id_for_prefix: None,
        session_title_for_prefix: None,
        plan_rev_number_by_id: &plan_rev_number_by_id,
        plan_preview_by_id: &plan_preview_by_id,
        amend_by_sha: &amend_by_sha,
        commit_preview_by_sha: &commit_preview_by_sha,
        feedback_target_by_id: &feedback_target_by_id,
        feedback_preview_by_id: &feedback_preview_by_id,
        feedback_kind_by_id: &feedback_kind_by_id,
        feedback_author_by_id: &feedback_author_by_id,
        feedback_status_by_author_kind: &feedback_status_by_author_kind,
    };
    let row = timeline_row_from_event(ev, &ctx);
    timeline_row_oob(&row, "timeline-feed")
}

fn layout_wide(title: &str, content: Markup) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) }
                style { (PreEscaped(STYLE)) }
            }
            body.wide-layout {
                main { (content) }
            }
        }
    }
}

fn layout(title: &str, content: Markup) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) }
                style { (PreEscaped(STYLE)) }
            }
            body {
                main { (content) }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{RowPresence, RowProjection, project_row};

    #[test]
    fn row_projection_is_a_pure_visibility_transition() {
        assert_eq!(
            project_row(RowPresence::NotRendered, RowPresence::Rendered),
            RowProjection::Insert
        );
        assert_eq!(
            project_row(RowPresence::Rendered, RowPresence::Rendered),
            RowProjection::Replace
        );
        assert_eq!(
            project_row(RowPresence::Rendered, RowPresence::NotRendered),
            RowProjection::Remove
        );
        assert_eq!(
            project_row(RowPresence::NotRendered, RowPresence::NotRendered),
            RowProjection::Noop
        );
    }
}
