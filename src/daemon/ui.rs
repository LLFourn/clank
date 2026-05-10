use maud::{DOCTYPE, Markup, PreEscaped, html};

use crate::storage::agents::Agent;
use crate::storage::events::Event;
use crate::storage::implementation_revisions::ImplementationRevision;
use crate::storage::plan_revisions::PlanRevision;
use crate::storage::plans::Plan;

pub struct HomeRow {
    pub plan: Plan,
    pub master_label: Option<String>,
    pub reviewer_labels: Vec<String>,
    pub latest_plan_revision_at: Option<i64>,
    pub pending_plan_count: i64,
    pub pending_impl_count: i64,
}

pub fn home(rows: &[HomeRow]) -> Markup {
    layout(
        "Trinity",
        html! {
            h1 { "Managed plans" }
            @if rows.is_empty() {
                p.empty {
                    "No plans registered yet. From a master agent, call "
                    code { "register_plan_file" } " in a shell pointed at the right repo."
                }
            }
            @if !rows.is_empty() {
                table.plans {
                    thead {
                        tr {
                            th { "Title" }
                            th { "Plan path" }
                            th { "Repo" }
                            th { "Master" }
                            th { "Reviewers" }
                            th { "State" }
                            th.num { "Plan" }
                            th.num { "Impl" }
                            th { "Last revision" }
                        }
                    }
                    tbody {
                        @for row in rows {
                            tr {
                                td { a href={ "/plans/" (row.plan.id) } { (display_title(&row.plan)) } }
                                td.path { (row.plan.plan_path.clone().unwrap_or_default()) }
                                td.path { (row.plan.repo_root) }
                                td { (row.master_label.clone().unwrap_or_else(|| "—".into())) }
                                td { (reviewer_summary(&row.reviewer_labels)) }
                                td { (state_badge(&row.plan.state)) }
                                td.num { (row.pending_plan_count) }
                                td.num { (row.pending_impl_count) }
                                td.relative { (relative_time(row.latest_plan_revision_at)) }
                            }
                        }
                    }
                }
            }
        },
    )
}

pub struct FeedbackItem {
    pub event_id: i64,
    pub actor: String,
    pub text: String,
    pub status: String,
    pub created_at: i64,
    pub target_label: Option<String>,
    pub outdated: bool,
}

pub struct PlanDetail {
    pub plan: Plan,
    pub agents: Vec<Agent>,
    pub revisions: Vec<PlanRevision>,
    pub latest_body_html: String,
    pub plan_feedback_pending: Vec<FeedbackItem>,
    pub plan_feedback_staged: Vec<FeedbackItem>,
    pub impl_revisions: Vec<ImplementationRevision>,
    pub impl_feedback_pending: Vec<FeedbackItem>,
    pub impl_feedback_staged: Vec<FeedbackItem>,
    pub timeline: Vec<TimelineItem>,
    /// Count of `plan_file_changed_after_implementation` events; non-zero
    /// means the human should archive/rename/reset before reusing the file.
    pub post_impl_drift_count: i64,
}

pub struct DiffViewFeedback {
    pub actor: String,
    pub text: String,
    pub status: String,
    pub created_at: i64,
}

pub struct CommitDiffView {
    pub plan_id: String,
    pub commit: ImplementationRevision,
    pub diff: String,
    pub feedback: Vec<DiffViewFeedback>,
}

pub struct TimelineItem {
    pub kind: String,
    pub actor: String,
    pub ts: i64,
    pub summary: String,
}

impl TimelineItem {
    pub fn from_event(ev: &Event, revisions: &[PlanRevision]) -> Self {
        let payload: serde_json::Value =
            serde_json::from_str(&ev.payload).unwrap_or(serde_json::Value::Null);
        let summary = match ev.kind.as_str() {
            "plan_revision_created" => {
                let rev_no = ev
                    .target_id
                    .as_deref()
                    .and_then(|id| id.parse::<i64>().ok())
                    .and_then(|id| revisions.iter().find(|r| r.id == id))
                    .map(|r| r.revision_number);
                match rev_no {
                    Some(n) => format!("plan revision #{n}"),
                    None => "plan revision".to_string(),
                }
            }
            "feedback_added" => {
                let text = payload.get("text").and_then(|v| v.as_str()).unwrap_or("");
                let preview = preview(text, 80);
                format!("feedback: \"{preview}\"")
            }
            "agent_joined" => format!("{} joined", ev.actor),
            "human_comment" => {
                let text = payload.get("text").and_then(|v| v.as_str()).unwrap_or("");
                format!("comment: \"{}\"", preview(text, 80))
            }
            "archived" => "plan archived".to_string(),
            "renamed" => format!(
                "renamed to \"{}\"",
                payload
                    .get("display_title")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
            ),
            "master_evicted" => "master evicted by curator".to_string(),
            "plan_file_missing" => "plan file went missing".to_string(),
            "plan_file_changed_after_implementation" => {
                "plan file edited after implementation — needs human lifecycle action (archive / fork / rename)".to_string()
            }
            other => other.to_string(),
        };
        Self {
            kind: ev.kind.clone(),
            actor: ev.actor.clone(),
            ts: ev.ts,
            summary,
        }
    }
}

fn preview(text: &str, n: usize) -> String {
    let collapsed: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.len() <= n {
        collapsed
    } else {
        format!("{}…", &collapsed.chars().take(n).collect::<String>())
    }
}

pub fn plan_detail(detail: &PlanDetail) -> Markup {
    let plan = &detail.plan;
    let plan_id = plan.id.clone();
    let total_pending = detail.plan_feedback_pending.len() + detail.plan_feedback_staged.len();
    layout(
        &format!("Trinity — {}", display_title(plan)),
        html! {
            nav.crumbs { a href="/" { "← all plans" } }
            h1 {
                (display_title(plan))
                " "
                (state_badge(&plan.state))
            }
            @if detail.post_impl_drift_count > 0 {
                div.banner.warning-banner {
                    strong { "Heads up:" }
                    " the watched plan file has been edited "
                    (detail.post_impl_drift_count) " time(s) after implementation was registered. "
                    "v0 ignores those edits — archive or rename this session if you want to start a new task using the same file."
                }
            }
            section.toolbar {
                @if plan.state == "planning" {
                    form method="post" action={ "/plans/" (plan_id) "/approve" } onsubmit="return confirm('Approve plan? Master proceeds to implementation.')" {
                        button.primary type="submit" { "Approve plan" }
                    }
                }
                @if matches!(plan.state.as_str(), "plan_approved" | "implementation_review") {
                    form method="post" action={ "/plans/" (plan_id) "/mark-done" } onsubmit="return confirm('Mark this plan done?')" {
                        button.primary type="submit" { "Mark done" }
                    }
                }
                form method="post" action={ "/plans/" (plan_id) "/archive" } onsubmit="return confirm('Archive this plan?')" {
                    button.danger type="submit" { "Archive" }
                }
                form.inline method="post" action={ "/plans/" (plan_id) "/rename" } {
                    input type="text" name="display_title" placeholder="new display title" required;
                    button type="submit" { "Rename" }
                }
                form method="post" action={ "/plans/" (plan_id) "/evict-master" } onsubmit="return confirm('Evict the current master agent?')" {
                    button.warning type="submit" { "Evict stale master" }
                }
            }
            section.meta {
                dl {
                    dt { "Plan path" }       dd.path { (plan.plan_path.clone().unwrap_or_default()) }
                    dt { "Repo root" }       dd.path { (plan.repo_root) }
                    dt { "Plan id" }         dd.mono { (plan.id) }
                    dt { "Joined agents" }   dd { (agents_widget(&detail.agents)) }
                }
            }
            section.plan-review {
                h2 { "Plan review" }
                @if let Some(latest) = detail.revisions.last() {
                    div.revision-meta {
                        "Latest revision: " strong { "#" (latest.revision_number) }
                        " · " (relative_time(Some(latest.created_at)))
                        " · detected by " code { (latest.detected_by) }
                    }
                    article.markdown {
                        (PreEscaped(detail.latest_body_html.clone()))
                    }
                    @if detail.revisions.len() > 1 {
                        details.revision-history {
                            summary { "Revision history (" (detail.revisions.len()) ")" }
                            ol reversed {
                                @for rev in detail.revisions.iter().rev() {
                                    li {
                                        "#" (rev.revision_number)
                                        " · " (relative_time(Some(rev.created_at)))
                                        " · " code { (rev.detected_by) }
                                    }
                                }
                            }
                        }
                    }
                } @else {
                    p.empty { "No plan revisions yet." }
                }

                h3 { "Feedback for the master (" (total_pending) ")" }
                @if total_pending == 0 {
                    p.empty { "No pending plan feedback. Reviewers can post via " code { "add_feedback" } "." }
                }
                @if !detail.plan_feedback_staged.is_empty() {
                    div.staged-bar {
                        form method="post" action={ "/plans/" (plan_id) "/deliver" } {
                            input type="hidden" name="target_kind" value="plan_revision";
                            button.primary type="submit" {
                                "Deliver " (detail.plan_feedback_staged.len()) " staged item(s) to master"
                            }
                        }
                    }
                }
                @for item in &detail.plan_feedback_staged {
                    (feedback_card(&plan_id, item))
                }
                @for item in &detail.plan_feedback_pending {
                    (feedback_card(&plan_id, item))
                }
            }
            section.impl-review {
                h2 { "Implementation review" }
                @let total_impl_pending = detail.impl_feedback_pending.len() + detail.impl_feedback_staged.len();
                @if let Some(latest) = detail.impl_revisions.last() {
                    div.commit-meta {
                        a href={ "/plans/" (plan_id) "/commits/" (latest.commit_sha) } {
                            "Latest commit "
                            code.mono { (latest.commit_sha.chars().take(12).collect::<String>()) }
                        }
                        @if let Some(branch) = &latest.branch {
                            " on " code { (branch) }
                        }
                        " · " (relative_time(Some(latest.created_at)))
                        @if latest.worktree_status.as_deref() == Some("clean") {
                            " · " span.muted { "worktree clean" }
                        } @else {
                            " · " span.warning { "dirty worktree at registration!" }
                        }
                    }
                    pre.commit-msg { (latest.commit_message) }
                    pre.diff-stat { (latest.diff_stat) }
                    @if detail.impl_revisions.len() > 1 {
                        details.commit-history {
                            summary { "Commit history (" (detail.impl_revisions.len()) ")" }
                            ol reversed {
                                @for rev in detail.impl_revisions.iter().rev() {
                                    li {
                                        a href={ "/plans/" (plan_id) "/commits/" (rev.commit_sha) } {
                                            code.mono { (rev.commit_sha.chars().take(12).collect::<String>()) }
                                        }
                                        " · " (relative_time(Some(rev.created_at)))
                                        " · " (rev.commit_message.lines().next().unwrap_or(""))
                                    }
                                }
                            }
                        }
                    }
                } @else {
                    p.empty {
                        "No implementation registered yet. Master agent calls "
                        code { "register_implementation_commit" } " when implementation is ready."
                    }
                    form method="post" action={ "/plans/" (plan_id) "/register-head" } onsubmit="return confirm('Register current HEAD of the repo as implementation review artifact?')" {
                        button type="submit" { "Start implementation review from current HEAD" }
                    }
                }

                h3 { "Feedback for the master (" (total_impl_pending) ")" }
                @if total_impl_pending == 0 {
                    p.empty {
                        @if detail.impl_revisions.is_empty() {
                            "No commit registered yet."
                        } @else {
                            "No pending implementation feedback."
                        }
                    }
                }
                @if !detail.impl_feedback_staged.is_empty() {
                    div.staged-bar {
                        form method="post" action={ "/plans/" (plan_id) "/deliver" } {
                            input type="hidden" name="target_kind" value="implementation_commit";
                            button.primary type="submit" {
                                "Deliver " (detail.impl_feedback_staged.len()) " staged item(s) to master"
                            }
                        }
                    }
                }
                @for item in &detail.impl_feedback_staged {
                    (feedback_card(&plan_id, item))
                }
                @for item in &detail.impl_feedback_pending {
                    (feedback_card(&plan_id, item))
                }
            }
            section.comment-box {
                h3 { "Add comment" }
                form method="post" action={ "/plans/" (plan_id) "/comment" } {
                    input type="text" name="author" placeholder="your name (optional)";
                    textarea name="text" rows="3" placeholder="comment visible to next agent fetch" required {}
                    button type="submit" { "Post comment" }
                }
            }
            section.timeline {
                h3 { "Timeline" }
                @if detail.timeline.is_empty() {
                    p.empty { "Empty." }
                }
                ol.timeline {
                    @for item in detail.timeline.iter().rev() {
                        li {
                            span.relative { (relative_time(Some(item.ts))) }
                            " "
                            span.actor { (item.actor) }
                            " · "
                            span.kind { (item.kind) }
                            " · "
                            span.summary { (item.summary) }
                        }
                    }
                }
            }
        },
    )
}

fn feedback_card(plan_id: &str, item: &FeedbackItem) -> Markup {
    let is_staged = item.status == "staged";
    let class = if item.outdated {
        "feedback outdated"
    } else if is_staged {
        "feedback staged"
    } else {
        "feedback pending"
    };
    html! {
        div.feedback class=(class) {
            header.feedback-head {
                span.actor { (item.actor) }
                " · "
                span.target { (item.target_label.clone().unwrap_or_else(|| "—".into())) }
                " · " span.relative { (relative_time(Some(item.created_at))) }
                " · " span.status { (item.status) }
                @if item.outdated {
                    " · " span.outdated-badge { "outdated" }
                }
            }
            div.feedback-body { pre { (item.text) } }
            div.feedback-actions {
                @if is_staged {
                    form method="post" action={ "/plans/" (plan_id) "/feedback/" (item.event_id) "/unstage" } {
                        button type="submit" { "Unstage" }
                    }
                } @else {
                    form method="post" action={ "/plans/" (plan_id) "/feedback/" (item.event_id) "/stage" } {
                        button.primary type="submit" { "Stage" }
                    }
                }
                details.edit-feedback {
                    summary { "Edit" }
                    form method="post" action={ "/plans/" (plan_id) "/feedback/" (item.event_id) "/edit" } {
                        textarea name="text" rows="4" required { (item.text) }
                        button type="submit" { "Save" }
                    }
                }
                form method="post" action={ "/plans/" (plan_id) "/feedback/" (item.event_id) "/delete" } onsubmit="return confirm('Delete this feedback?')" {
                    button.danger type="submit" { "Delete" }
                }
            }
        }
    }
}

pub fn commit_diff(view: &CommitDiffView) -> Markup {
    let short = view.commit.commit_sha.chars().take(12).collect::<String>();
    layout(
        &format!("Trinity — commit {}", short),
        html! {
            nav.crumbs {
                a href="/" { "← all plans" }
                " · "
                a href={ "/plans/" (view.plan_id) } { "back to plan" }
            }
            h1 {
                "Commit "
                code.mono { (short) }
            }
            section.meta {
                dl {
                    dt { "Full SHA" }    dd.mono { (view.commit.commit_sha) }
                    dt { "Parent" }      dd.mono { (view.commit.parent_sha.clone().unwrap_or_else(|| "(root)".into())) }
                    dt { "Branch" }      dd { (view.commit.branch.clone().unwrap_or_else(|| "(detached)".into())) }
                    dt { "is HEAD?" }    dd { (if view.commit.is_head != 0 { "yes" } else { "no" }) }
                    dt { "Worktree" }    dd { (view.commit.worktree_status.clone().unwrap_or_else(|| "unknown".into())) }
                    dt { "Registered" }  dd { (view.commit.registered_by) " · " (relative_time(Some(view.commit.created_at))) }
                }
            }
            section.commit-message {
                h2 { "Commit message" }
                pre.commit-msg { (view.commit.commit_message) }
            }
            section.commit-stat {
                h2 { "Diff stat" }
                pre.diff-stat { (view.commit.diff_stat) }
            }
            section.commit-diff {
                h2 { "Diff" }
                pre.diff-text { (view.diff) }
            }
            @if !view.feedback.is_empty() {
                section.commit-feedback {
                    h2 { "Feedback on this commit" }
                    @for fb in &view.feedback {
                        div.feedback {
                            header.feedback-head {
                                span.actor { (fb.actor) }
                                " · " span.relative { (relative_time(Some(fb.created_at))) }
                                " · " span.status { (fb.status) }
                            }
                            div.feedback-body { pre { (fb.text) } }
                        }
                    }
                }
            }
        },
    )
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
                script src="https://unpkg.com/htmx.org@2.0.4" {}
            }
            body {
                main {
                    (content)
                }
            }
        }
    }
}

fn display_title(plan: &Plan) -> String {
    plan.display_title
        .clone()
        .or_else(|| {
            plan.plan_path.as_ref().and_then(|p| {
                std::path::Path::new(p)
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
            })
        })
        .unwrap_or_else(|| plan.id.chars().take(8).collect::<String>())
}

fn state_badge(state: &str) -> Markup {
    let class = match state {
        "planning" => "badge planning",
        "plan_approved" => "badge plan-approved",
        "implementation_review" => "badge impl-review",
        "done" => "badge done",
        "archived" => "badge archived",
        _ => "badge",
    };
    html! { span class=(class) { (state) } }
}

fn agents_widget(agents: &[Agent]) -> Markup {
    if agents.is_empty() {
        return html! { span.muted { "—" } };
    }
    html! {
        ul.agents {
            @for a in agents {
                li {
                    span.role { (a.role) ": " }
                    span.label { (a.label) }
                    " " (relative_time(Some(a.last_seen)))
                }
            }
        }
    }
}

fn reviewer_summary(labels: &[String]) -> String {
    if labels.is_empty() {
        "—".to_string()
    } else {
        labels.join(", ")
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
        return format!("{diff}s ago");
    }
    if diff < 3600 {
        return format!("{}m ago", diff / 60);
    }
    if diff < 86400 {
        return format!("{}h ago", diff / 3600);
    }
    format!("{}d ago", diff / 86400)
}

const STYLE: &str = r#"
:root {
  --bg: #fafaf7;
  --bg-alt: #ffffff;
  --fg: #1f1f1f;
  --muted: #6c6c6c;
  --line: #d8d8d4;
  --accent: #2b3a55;
  --planning: #ad6b00;
  --plan-approved: #1f6f43;
  --impl-review: #1d5fb0;
  --done: #555;
  --archived: #999;
}
* { box-sizing: border-box; }
body {
  margin: 0;
  font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", system-ui, sans-serif;
  background: var(--bg);
  color: var(--fg);
  line-height: 1.55;
}
main { max-width: 1100px; margin: 0 auto; padding: 24px; }
h1 { font-size: 1.6rem; margin: 0 0 16px; font-weight: 600; }
h2 { font-size: 1.15rem; margin: 28px 0 12px; font-weight: 600; }
h3 { font-size: 1rem; margin: 22px 0 10px; font-weight: 600; color: #333; }
nav.crumbs { font-size: 0.9rem; margin-bottom: 12px; }
nav.crumbs a { color: var(--muted); text-decoration: none; }
nav.crumbs a:hover { text-decoration: underline; }
table.plans {
  width: 100%; border-collapse: collapse; background: var(--bg-alt);
  border: 1px solid var(--line); border-radius: 6px; overflow: hidden;
}
table.plans th, table.plans td {
  padding: 10px 14px; text-align: left; border-bottom: 1px solid var(--line);
  font-size: 0.92rem; vertical-align: top;
}
table.plans th { background: #f3f3ee; font-weight: 600; font-size: 0.82rem; text-transform: uppercase; letter-spacing: 0.04em; color: var(--muted); }
table.plans tr:last-child td { border-bottom: none; }
.num { text-align: right; font-variant-numeric: tabular-nums; }
.path, .mono { font-family: ui-monospace, "SF Mono", monospace; font-size: 0.83rem; color: var(--muted); word-break: break-all; }
.empty { color: var(--muted); font-style: italic; }
.badge { display: inline-block; padding: 2px 8px; border-radius: 999px; font-size: 0.78rem; font-weight: 600; text-transform: uppercase; letter-spacing: 0.04em; background: #eee; color: #444; }
.badge.planning { background: #fff1d6; color: var(--planning); }
.badge.plan-approved { background: #d8f0e1; color: var(--plan-approved); }
.badge.impl-review { background: #d6e5f7; color: var(--impl-review); }
.badge.done { background: #e6e6e6; color: var(--done); }
.badge.archived { background: #efefef; color: var(--archived); }
.relative { color: var(--muted); font-size: 0.85rem; }
section.meta { background: var(--bg-alt); border: 1px solid var(--line); border-radius: 6px; padding: 14px 18px; margin-bottom: 18px; }
section.meta dl { display: grid; grid-template-columns: 130px 1fr; gap: 4px 16px; margin: 0; }
section.meta dt { color: var(--muted); font-size: 0.82rem; text-transform: uppercase; letter-spacing: 0.04em; }
section.meta dd { margin: 0; }
section.toolbar { display: flex; gap: 8px; flex-wrap: wrap; margin-bottom: 14px; }
section.toolbar form { display: inline-flex; gap: 4px; }
section.toolbar form.inline input { padding: 4px 8px; border: 1px solid var(--line); border-radius: 4px; font-size: 0.88rem; }
button { padding: 4px 12px; border: 1px solid var(--line); border-radius: 4px; background: var(--bg-alt); color: var(--fg); cursor: pointer; font-size: 0.88rem; }
button:hover { background: #efefe9; }
button.primary { background: var(--accent); color: #fff; border-color: var(--accent); }
button.primary:hover { background: #1f2a3d; }
button.danger { color: #b03030; border-color: #d8a8a8; }
button.danger:hover { background: #fff0f0; }
button.warning { color: #855900; border-color: #d8c98a; }
ul.agents { list-style: none; padding: 0; margin: 0; display: flex; flex-wrap: wrap; gap: 8px; }
ul.agents li { background: #efefe9; padding: 2px 8px; border-radius: 4px; font-size: 0.85rem; }
ul.agents .role { color: var(--muted); font-size: 0.78rem; text-transform: uppercase; letter-spacing: 0.04em; }
.muted { color: var(--muted); }
article.markdown {
  background: var(--bg-alt); border: 1px solid var(--line); border-radius: 6px;
  padding: 18px 22px; font-size: 0.95rem;
}
article.markdown pre { background: #f3f3ee; padding: 10px 12px; border-radius: 4px; overflow-x: auto; font-size: 0.85rem; }
article.markdown code { background: #f3f3ee; padding: 1px 4px; border-radius: 3px; font-size: 0.85rem; font-family: ui-monospace, "SF Mono", monospace; }
article.markdown pre code { background: none; padding: 0; }
article.markdown h1 { font-size: 1.3rem; }
article.markdown h2 { font-size: 1.1rem; margin-top: 20px; }
article.markdown h3 { font-size: 1rem; margin-top: 16px; }
article.markdown p { margin: 8px 0; }
.revision-meta { color: var(--muted); font-size: 0.85rem; margin-bottom: 8px; }
.revision-history { margin-top: 16px; }
.revision-history summary { cursor: pointer; color: var(--muted); font-size: 0.9rem; }
.revision-history ol { margin: 8px 0; padding-left: 20px; font-size: 0.88rem; color: var(--muted); }

div.staged-bar { background: #fff8e0; border: 1px solid #e6cf80; border-radius: 6px; padding: 10px 14px; margin-bottom: 14px; }
div.feedback {
  background: var(--bg-alt); border: 1px solid var(--line); border-radius: 6px;
  padding: 12px 14px; margin-bottom: 10px;
}
div.feedback.staged { border-color: #c0a040; background: #fffbe8; }
header.feedback-head { font-size: 0.85rem; color: var(--muted); margin-bottom: 6px; }
header.feedback-head .actor { font-weight: 600; color: var(--fg); }
header.feedback-head .status { text-transform: uppercase; letter-spacing: 0.05em; font-weight: 600; }
div.feedback-body pre { white-space: pre-wrap; word-wrap: break-word; background: none; padding: 0; margin: 0; font-family: inherit; font-size: 0.95rem; }
div.feedback-actions { margin-top: 10px; display: flex; gap: 8px; align-items: center; }
div.feedback-actions form { display: inline; }
details.edit-feedback summary { cursor: pointer; font-size: 0.85rem; color: var(--muted); padding: 4px 8px; }
details.edit-feedback textarea { width: 100%; padding: 8px; border: 1px solid var(--line); border-radius: 4px; font-family: inherit; font-size: 0.9rem; box-sizing: border-box; }
section.comment-box { margin-top: 24px; }
section.comment-box form { display: flex; flex-direction: column; gap: 6px; max-width: 600px; }
section.comment-box textarea, section.comment-box input { padding: 8px; border: 1px solid var(--line); border-radius: 4px; font-family: inherit; font-size: 0.92rem; }
section.comment-box button { align-self: flex-start; }
section.timeline ol { list-style: none; padding: 0; margin: 0; }
section.timeline li { padding: 6px 0; font-size: 0.88rem; border-bottom: 1px dashed var(--line); }
section.timeline .actor { font-weight: 600; }
section.timeline .kind { color: var(--muted); font-family: ui-monospace, "SF Mono", monospace; }
div.banner { padding: 12px 16px; border-radius: 6px; margin-bottom: 14px; font-size: 0.92rem; }
div.banner.warning-banner { background: #fff4d6; border: 1px solid #d8b863; color: #6c4a00; }
.outdated-badge { background: #f3e1c0; color: #855900; padding: 1px 6px; border-radius: 4px; font-size: 0.78rem; }
.warning { color: #855900; }
"#;
