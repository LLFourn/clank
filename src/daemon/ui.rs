use std::collections::HashMap;

use maud::{DOCTYPE, Markup, PreEscaped, html};

use crate::storage::agents::Agent;
use crate::storage::events::Event;
use crate::storage::feedback::FeedbackRecord;
use crate::storage::implementation_revisions::ImplementationRevision;
use crate::storage::plan_revisions::PlanRevision;
use crate::storage::plans::Plan;
use crate::storage::sessions::Session;

// ---------- Home (session list) ----------

pub struct SessionRow {
    pub session: Session,
    pub recent_agents: Vec<String>,
    pub active_plan_state: Option<String>,
    pub plan_feedback_count: i64,
    pub impl_feedback_count: i64,
}

pub fn home(rows: &[SessionRow]) -> Markup {
    layout(
        "Trinity",
        html! {
            h1 { "Sessions" }
            @if rows.is_empty() {
                p.empty {
                    "No sessions yet. From an agent, call "
                    code { "register_plan_file" } " with a `session_id` slug to create one."
                }
            }
            @if !rows.is_empty() {
                table.sessions {
                    thead {
                        tr {
                            th { "Session" }
                            th { "Title" }
                            th { "Plan path" }
                            th { "Repo" }
                            th { "Agents" }
                            th { "Active plan" }
                            th.num { "Plan fb" }
                            th.num { "Impl fb" }
                            th { "Updated" }
                        }
                    }
                    tbody {
                        @for row in rows {
                            tr {
                                td.mono { a href={ "/sessions/" (row.session.id) } { (row.session.id) } }
                                td { (row.session.display_title.clone().unwrap_or_default()) }
                                td.path { (row.session.plan_file_path) }
                                td.path { (row.session.repo_root) }
                                td { (agent_summary(&row.recent_agents)) }
                                td { (active_plan_badge(row.active_plan_state.as_deref())) }
                                td.num { (row.plan_feedback_count) }
                                td.num { (row.impl_feedback_count) }
                                td.relative { (relative_time(Some(row.session.updated_at))) }
                            }
                        }
                    }
                }
            }
        },
    )
}

// ---------- Session detail (active plan + history summary) ----------

pub struct FeedbackItem {
    #[allow(dead_code)]
    pub feedback_id: i64,
    pub author_label: String,
    pub body: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub target_label: Option<String>,
    pub outdated: bool,
}

pub struct SessionDetail {
    pub session: Session,
    pub agents: Vec<Agent>,
    pub active_plan: Option<Plan>,
    pub plan_revisions: Vec<PlanRevision>,
    pub latest_body_html: String,
    pub impl_revisions: Vec<ImplementationRevision>,
    pub plan_feedback: Vec<FeedbackItem>,
    pub impl_feedback: Vec<FeedbackItem>,
    pub archived_count: i64,
    pub timeline: Vec<TimelineItem>,
}

pub struct DiffViewFeedback {
    pub author_label: String,
    pub body: String,
    pub created_at: i64,
}

pub struct CommitDiffView {
    pub session_id: String,
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
    /// Render a single audit event into a timeline row. Feedback events
    /// look up the structural row in `feedback_by_id` so the body excerpt
    /// comes from `feedback.body`, not `events.payload`.
    pub fn from_event(
        ev: &Event,
        revisions: &[PlanRevision],
        feedback_by_id: &HashMap<i64, FeedbackRecord>,
    ) -> Self {
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
            "feedback_added" => summarise_feedback_event(&payload, feedback_by_id, "feedback"),
            "feedback_updated" => {
                summarise_feedback_event(&payload, feedback_by_id, "feedback updated")
            }
            "agent_joined" => format!("{} joined", ev.actor),
            "human_comment" => {
                let text = payload
                    .get("comment")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                format!("comment: \"{}\"", preview(text, 80))
            }
            "state_transition" => {
                let from = payload.get("from").and_then(|v| v.as_str());
                let to = payload.get("to").and_then(|v| v.as_str());
                match (from, to) {
                    (Some(f), Some(t)) => format!("state: {f} → {t}"),
                    (_, Some(t)) => format!("state → {t}"),
                    _ => "state transition".to_string(),
                }
            }
            "impl_revision_created" => format!(
                "implementation commit {}",
                ev.target_id.as_deref().map(short).unwrap_or_default()
            ),
            "dirty_worktree_warning" => "dirty worktree at commit registration".to_string(),
            "renamed" => format!(
                "renamed to \"{}\"",
                payload
                    .get("display_title")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
            ),
            "plan_file_missing" => "plan file went missing".to_string(),
            other => other.to_string(),
        };
        Self {
            kind: ev.kind.clone(),
            actor: ev.actor.clone(),
            ts: ev.ts,
            summary,
        }
    }

    /// Audit-only access to `prior_body` on `feedback_updated` events.
    /// This is the only production path that reads from `events.payload`
    /// (besides `state_transition`/`renamed`/`human_comment` housekeeping
    /// fields, which never carry feedback content).
    #[allow(dead_code)]
    pub fn feedback_updated_prior_body(payload: &serde_json::Value) -> Option<String> {
        payload
            .get("prior_body")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    }
}

fn summarise_feedback_event(
    payload: &serde_json::Value,
    feedback_by_id: &HashMap<i64, FeedbackRecord>,
    prefix: &str,
) -> String {
    let id = payload.get("feedback_id").and_then(|v| v.as_i64());
    match id.and_then(|id| feedback_by_id.get(&id)) {
        Some(record) => format!("{prefix}: \"{}\"", preview(&record.body, 80)),
        None => match id {
            Some(id) => format!("{prefix} #{id} (no longer available)"),
            None => format!("{prefix} (no id)"),
        },
    }
}

fn preview(text: &str, n: usize) -> String {
    let collapsed: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= n {
        collapsed
    } else {
        format!("{}…", collapsed.chars().take(n).collect::<String>())
    }
}

fn short(sha: &str) -> String {
    sha.chars().take(12).collect()
}

pub fn session_detail(d: &SessionDetail) -> Markup {
    let session_id = d.session.id.clone();
    let total_plan = d.plan_feedback.len();
    let total_impl = d.impl_feedback.len();
    layout(
        &format!("Trinity — {}", session_title(&d.session)),
        html! {
            nav.crumbs { a href="/" { "← all sessions" } " · " a href={ "/sessions/" (session_id) "/history" } { "history (" (d.archived_count) ")" } }
            h1 {
                (session_title(&d.session)) " " span.mono.session-id { "(" (d.session.id) ")" }
                " " (active_plan_badge(d.active_plan.as_ref().map(|p| p.state.as_str())))
            }
            section.toolbar {
                @if d.active_plan.is_some() {
                    form method="post" action={ "/sessions/" (session_id) "/archive" } onsubmit="return confirm('Archive the active plan? Session continues; new plan will need register_plan_file.')" {
                        button.danger type="submit" { "Archive active plan" }
                    }
                }
                form.inline method="post" action={ "/sessions/" (session_id) "/rename" } {
                    input type="text" name="display_title" placeholder="new display title" required;
                    button type="submit" { "Rename" }
                }
            }
            section.meta {
                dl {
                    dt { "Plan file" }      dd.path { (d.session.plan_file_path) }
                    dt { "Repo root" }      dd.path { (d.session.repo_root) }
                    @if let Some(plan) = &d.active_plan {
                        dt { "Active plan id" } dd.mono { (plan.id) }
                        dt { "Base commit" }    dd.mono { (short(&plan.base_commit)) }
                    }
                    dt { "Joined agents" }  dd { (agents_widget(&d.agents)) }
                }
            }

            section.plan-review {
                h2 { "Plan review" }
                @if let Some(plan) = &d.active_plan {
                    @if let Some(latest) = d.plan_revisions.last() {
                        div.revision-meta {
                            "Latest revision: " strong { "#" (latest.revision_number) }
                            " · " (relative_time(Some(latest.created_at)))
                        }
                        article.markdown { (PreEscaped(d.latest_body_html.clone())) }
                        @if d.plan_revisions.len() > 1 {
                            details.revision-history {
                                summary { "Revision history (" (d.plan_revisions.len()) ")" }
                                ol reversed {
                                    @for rev in d.plan_revisions.iter().rev() {
                                        li {
                                            "#" (rev.revision_number)
                                            " · " (relative_time(Some(rev.created_at)))
                                        }
                                    }
                                }
                            }
                        }
                    }
                    h3 { "Plan feedback (" (total_plan) ")" }
                    @if total_plan == 0 {
                        p.empty { "No plan feedback yet." }
                    }
                    @for item in &d.plan_feedback {
                        (feedback_card(item))
                    }
                    @let _ = plan;
                } @else {
                    p.empty {
                        "No active plan. Master agent calls "
                        code { "register_plan_file" }
                        " with this session_id to start a new lifecycle."
                    }
                }
            }

            section.impl-review {
                h2 { "Implementation review" }
                @if let Some(latest) = d.impl_revisions.last() {
                    div.commit-meta {
                        a href={ "/sessions/" (session_id) "/commits/" (latest.commit_sha) } {
                            "Latest commit " code.mono { (short(&latest.commit_sha)) }
                        }
                        @if let Some(b) = &latest.branch {
                            " on " code { (b) }
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
                    @if d.impl_revisions.len() > 1 {
                        details.commit-history {
                            summary { "Commit history (" (d.impl_revisions.len()) ")" }
                            ol reversed {
                                @for rev in d.impl_revisions.iter().rev() {
                                    li {
                                        a href={ "/sessions/" (session_id) "/commits/" (rev.commit_sha) } {
                                            code.mono { (short(&rev.commit_sha)) }
                                        }
                                        " · " (relative_time(Some(rev.created_at)))
                                        " · " (rev.commit_message.lines().next().unwrap_or(""))
                                    }
                                }
                            }
                        }
                    }
                } @else if d.active_plan.is_some() {
                    p.empty {
                        "No implementation yet. Master agent commits and calls "
                        code { "register_implementation_commit" } "."
                    }
                    form method="post" action={ "/sessions/" (session_id) "/register-head" } onsubmit="return confirm('Register current HEAD as the implementation commit?')" {
                        button type="submit" { "Start implementation review from current HEAD" }
                    }
                }

                @if d.active_plan.is_some() {
                    h3 { "Implementation feedback (" (total_impl) ")" }
                    @if total_impl == 0 {
                        p.empty { "No impl feedback yet." }
                    }
                    @for item in &d.impl_feedback {
                        (feedback_card(item))
                    }
                }
            }

            section.comment-box {
                h3 { "Add comment" }
                form method="post" action={ "/sessions/" (session_id) "/comment" } {
                    input type="text" name="author" placeholder="your name (optional)";
                    textarea name="text" rows="3" placeholder="comment for the timeline" required {}
                    button type="submit" { "Post comment" }
                }
            }

            section.timeline {
                h3 { "Timeline" }
                @if d.timeline.is_empty() {
                    p.empty { "Empty." }
                }
                ol.timeline {
                    @for item in d.timeline.iter().rev() {
                        li {
                            span.relative { (relative_time(Some(item.ts))) } " "
                            span.actor { (item.actor) } " · "
                            span.kind { (item.kind) } " · "
                            span.summary { (item.summary) }
                        }
                    }
                }
            }
        },
    )
}

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
    layout(
        &format!("Trinity — commit {}", short_sha),
        html! {
            nav.crumbs {
                a href="/" { "← all sessions" } " · "
                a href={ "/sessions/" (view.session_id) } { "session" }
            }
            h1 { "Commit " code.mono { (short_sha) } }
            section.meta {
                dl {
                    dt { "Full SHA" }   dd.mono { (view.commit.commit_sha) }
                    dt { "Parent" }     dd.mono { (view.commit.parent_sha.clone().unwrap_or_else(|| "(root)".into())) }
                    dt { "Branch" }     dd { (view.commit.branch.clone().unwrap_or_else(|| "(detached)".into())) }
                    dt { "is HEAD?" }   dd { (if view.commit.is_head != 0 { "yes" } else { "no" }) }
                    dt { "Worktree" }   dd { (view.commit.worktree_status.clone().unwrap_or_else(|| "unknown".into())) }
                    dt { "Registered" } dd { (view.commit.registered_by) " · " (relative_time(Some(view.commit.created_at))) }
                }
            }
            section.commit-message { h2 { "Commit message" } pre.commit-msg { (view.commit.commit_message) } }
            section.commit-stat { h2 { "Diff stat" } pre.diff-stat { (view.commit.diff_stat) } }
            section.commit-diff { h2 { "Diff" } pre.diff-text { (view.diff) } }
            @if !view.feedback.is_empty() {
                section.commit-feedback {
                    h2 { "Feedback on this commit" }
                    @for fb in &view.feedback {
                        div.feedback {
                            header.feedback-head {
                                span.actor { (fb.author_label) }
                                " · " span.relative { (relative_time(Some(fb.created_at))) }
                            }
                            div.feedback-body { pre { (fb.body) } }
                        }
                    }
                }
            }
        },
    )
}

// ---------- helpers ----------

fn feedback_card(item: &FeedbackItem) -> Markup {
    let class = if item.outdated {
        "feedback outdated"
    } else {
        "feedback"
    };
    let updated = item.updated_at != item.created_at;
    html! {
        div.feedback class=(class) {
            header.feedback-head {
                span.actor { (item.author_label) }
                " · " span.target { (item.target_label.clone().unwrap_or_else(|| "—".into())) }
                " · " span.relative { (relative_time(Some(item.created_at))) }
                @if updated {
                    " · " span.muted { "updated " (relative_time(Some(item.updated_at))) }
                }
                @if item.outdated {
                    " · " span.outdated-badge { "outdated" }
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

fn agents_widget(agents: &[Agent]) -> Markup {
    if agents.is_empty() {
        return html! { span.muted { "—" } };
    }
    html! {
        ul.agents {
            @for a in agents {
                li {
                    span.label { (a.label) }
                    " " (relative_time(Some(a.last_seen)))
                }
            }
        }
    }
}

fn agent_summary(labels: &[String]) -> String {
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

const STYLE: &str = r#"
:root {
  --bg: #fafaf7; --bg-alt: #ffffff; --fg: #1f1f1f; --muted: #6c6c6c;
  --line: #d8d8d4; --accent: #2b3a55;
  --planning: #ad6b00; --impl: #1d5fb0; --archived: #999;
}
* { box-sizing: border-box; }
body { margin: 0; font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", system-ui, sans-serif;
       background: var(--bg); color: var(--fg); line-height: 1.55; }
main { max-width: 1100px; margin: 0 auto; padding: 24px; }
h1 { font-size: 1.6rem; margin: 0 0 16px; font-weight: 600; }
h2 { font-size: 1.15rem; margin: 28px 0 12px; font-weight: 600; }
h3 { font-size: 1rem; margin: 22px 0 10px; font-weight: 600; color: #333; }
nav.crumbs { font-size: 0.9rem; margin-bottom: 12px; }
nav.crumbs a { color: var(--muted); text-decoration: none; }
nav.crumbs a:hover { text-decoration: underline; }
table.sessions, table.history { width: 100%; border-collapse: collapse; background: var(--bg-alt);
  border: 1px solid var(--line); border-radius: 6px; overflow: hidden; }
table th, table td { padding: 10px 14px; text-align: left; border-bottom: 1px solid var(--line);
  font-size: 0.92rem; vertical-align: top; }
table th { background: #f3f3ee; font-weight: 600; font-size: 0.82rem; text-transform: uppercase;
  letter-spacing: 0.04em; color: var(--muted); }
table tr:last-child td { border-bottom: none; }
.num { text-align: right; font-variant-numeric: tabular-nums; }
.path, .mono { font-family: ui-monospace, "SF Mono", monospace; font-size: 0.83rem; color: var(--muted); word-break: break-all; }
.session-id { font-size: 0.85rem; color: var(--muted); }
.empty { color: var(--muted); font-style: italic; }
.badge { display: inline-block; padding: 2px 8px; border-radius: 999px; font-size: 0.78rem; font-weight: 600;
  text-transform: uppercase; letter-spacing: 0.04em; background: #eee; color: #444; }
.badge.planning { background: #fff1d6; color: var(--planning); }
.badge.impl-review { background: #d6e5f7; color: var(--impl); }
.badge.archived { background: #efefef; color: var(--archived); }
.relative { color: var(--muted); font-size: 0.85rem; }
section.meta { background: var(--bg-alt); border: 1px solid var(--line); border-radius: 6px;
  padding: 14px 18px; margin-bottom: 18px; }
section.meta dl { display: grid; grid-template-columns: 130px 1fr; gap: 4px 16px; margin: 0; }
section.meta dt { color: var(--muted); font-size: 0.82rem; text-transform: uppercase; letter-spacing: 0.04em; }
section.meta dd { margin: 0; }
section.toolbar { display: flex; gap: 8px; flex-wrap: wrap; margin-bottom: 14px; }
section.toolbar form { display: inline-flex; gap: 4px; }
section.toolbar form.inline input { padding: 4px 8px; border: 1px solid var(--line); border-radius: 4px; font-size: 0.88rem; }
button { padding: 4px 12px; border: 1px solid var(--line); border-radius: 4px; background: var(--bg-alt);
  color: var(--fg); cursor: pointer; font-size: 0.88rem; }
button:hover { background: #efefe9; }
button.primary { background: var(--accent); color: #fff; border-color: var(--accent); }
button.primary:hover { background: #1f2a3d; }
button.danger { color: #b03030; border-color: #d8a8a8; }
button.warning { color: #855900; border-color: #d8c98a; }
ul.agents { list-style: none; padding: 0; margin: 0; display: flex; flex-wrap: wrap; gap: 8px; }
ul.agents li { background: #efefe9; padding: 2px 8px; border-radius: 4px; font-size: 0.85rem; }
ul.agents .role { color: var(--muted); font-size: 0.78rem; text-transform: uppercase; }
.muted { color: var(--muted); }
article.markdown { background: var(--bg-alt); border: 1px solid var(--line); border-radius: 6px;
  padding: 18px 22px; font-size: 0.95rem; }
article.markdown pre { background: #f3f3ee; padding: 10px 12px; border-radius: 4px; overflow-x: auto; font-size: 0.85rem; }
article.markdown code { background: #f3f3ee; padding: 1px 4px; border-radius: 3px; font-size: 0.85rem; font-family: ui-monospace, "SF Mono", monospace; }
article.markdown pre code { background: none; padding: 0; }
.revision-meta { color: var(--muted); font-size: 0.85rem; margin-bottom: 8px; }
details.revision-history, details.commit-history { margin-top: 16px; }
details.revision-history summary, details.commit-history summary { cursor: pointer; color: var(--muted); font-size: 0.9rem; }
details.revision-history ol, details.commit-history ol { margin: 8px 0; padding-left: 20px; font-size: 0.88rem; color: var(--muted); }
div.feedback { background: var(--bg-alt); border: 1px solid var(--line); border-radius: 6px;
  padding: 12px 14px; margin-bottom: 10px; }
div.feedback.outdated { border-color: #f3e1c0; background: #fdf6e7; }
header.feedback-head { font-size: 0.85rem; color: var(--muted); margin-bottom: 6px; }
header.feedback-head .actor { font-weight: 600; color: var(--fg); }
.outdated-badge { background: #f3e1c0; color: #855900; padding: 1px 6px; border-radius: 4px; font-size: 0.78rem; }
.warning { color: #855900; }
div.feedback-body pre { white-space: pre-wrap; word-wrap: break-word; background: none; padding: 0; margin: 0;
  font-family: inherit; font-size: 0.95rem; }
section.comment-box { margin-top: 24px; }
section.comment-box form { display: flex; flex-direction: column; gap: 6px; max-width: 600px; }
section.comment-box textarea, section.comment-box input { padding: 8px; border: 1px solid var(--line);
  border-radius: 4px; font-family: inherit; font-size: 0.92rem; }
section.comment-box button { align-self: flex-start; }
section.timeline ol { list-style: none; padding: 0; margin: 0; }
section.timeline li { padding: 6px 0; font-size: 0.88rem; border-bottom: 1px dashed var(--line); }
section.timeline .actor { font-weight: 600; }
section.timeline .kind { color: var(--muted); font-family: ui-monospace, "SF Mono", monospace; }
"#;
