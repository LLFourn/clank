use std::collections::HashMap;

use maud::{DOCTYPE, Markup, PreEscaped, html};

use crate::daemon::service::FeedbackFileSnapshot;
use crate::domain::FeedbackFileStatus;
use crate::storage::events::Event;
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
    /// Number of feedback files in `current` status for this session.
    pub current_feedback_files: i64,
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
                            th.num { "Files" }
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
                                td.num { (row.current_feedback_files) }
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
    pub author_label: String,
    pub body: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub target_label: Option<String>,
}

pub struct SessionDetail {
    pub session: Session,
    pub active_plan: Option<Plan>,
    pub archived_count: i64,
    /// Raw audit events for the session, oldest-first.
    pub events: Vec<Event>,
    /// Pre-derived snapshots from `SessionService::build_feedback_context`.
    pub plan_feedback_files: Vec<FeedbackFileSnapshot>,
    pub impl_feedback_files: Vec<FeedbackFileSnapshot>,
    pub git_logs_head_path: Option<String>,
    pub plan_feedback_dir_path: Option<String>,
    pub impl_feedback_dir_path: Option<String>,
    /// Lookup: commit_sha → amend classification. Populated for all impl
    /// revisions across all plans in this session.
    pub amend_by_sha: HashMap<String, crate::daemon::amend::AmendInfo>,
    /// Lookup: plan_revision id → revision_number, for routing the
    /// timeline's plan-revision rows to per-revision URLs.
    pub plan_rev_number_by_id: HashMap<i64, i64>,
    /// Lookup: feedback id → (kind, target). Used to wire the timeline's
    /// feedback rows to the right artifact page + anchor.
    pub feedback_target_by_id: HashMap<i64, (String, String)>,
    /// The maximum event id at render time, embedded in the SSE
    /// `?since=` so the live stream picks up from where the page rendered.
    pub max_event_id: i64,
}

pub struct DiffViewFeedback {
    pub feedback_id: i64,
    pub author_label: String,
    pub feedback_kind: String,
    pub file_status: Option<FeedbackFileStatus>,
    pub file_path: Option<String>,
    pub body_html: String,
    pub created_at: i64,
}

pub struct CommitDiffView {
    pub session_id: String,
    pub commit: ImplementationRevision,
    pub diff: String,
    /// Which base SHA the diff was computed against. Rendered prominently in
    /// the sticky header — `Parent(None)` means root commit (no parent).
    pub base_label: super::http::DiffBaseLabel,
    pub feedback: Vec<DiffViewFeedback>,
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
    html! {
        section.watched-artifacts {
            h2 { "Watched artifacts" }
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
    let ctx = TimelineRenderCtx {
        session_id: &session_id,
        plan_rev_number_by_id: &d.plan_rev_number_by_id,
        amend_by_sha: &d.amend_by_sha,
        feedback_target_by_id: &d.feedback_target_by_id,
    };
    layout(
        &format!("Trinity — {}", session_title(&d.session)),
        html! {
            (timeline_head_scripts())
            header.session-head {
                div.session-head-left {
                    a.back href="/" { "← all sessions" }
                    h1.session-id { (session_id) }
                    (active_plan_badge(d.active_plan.as_ref().map(|p| p.state.as_str())))
                    @if d.archived_count > 0 {
                        a.muted-link href={ "/sessions/" (session_id) "/history" } { "history (" (d.archived_count) ")" }
                    }
                }
                div.session-head-right {
                    @if let Some(plan) = &d.active_plan {
                        span.head-meta { "base " code.mono { (short(&plan.base_commit)) } }
                    }
                    span.head-meta { "plan: " span.path.mono { (d.session.plan_file_path) } }
                }
            }

            (watched_artifacts_strip(d))

            section.timeline-section {
                h2 { "Timeline" }
                @if d.events.is_empty() {
                    p.empty { "Nothing has happened yet." }
                }
                div.timeline-wrap {
                    section id="timeline-feed"
                            hx-ext="sse"
                            sse-connect={ "/sessions/" (session_id) "/events?since=" (d.max_event_id) }
                            sse-swap="message"
                            hx-swap="none" {
                        @for ev in d.events.iter().rev() {
                            (timeline_entry_full(ev, &ctx))
                        }
                    }
                }
            }
        },
    )
}

/// The minimum lookup context needed to render one timeline row with
/// click-through actions. Shared between the initial page render (built
/// from `SessionDetail`) and the SSE per-event renderer (built per-event
/// from focused DB lookups). Without this, the SSE fragment loses the
/// action pills and the live timeline isn't actually click-through.
pub struct TimelineRenderCtx<'a> {
    pub session_id: &'a str,
    pub plan_rev_number_by_id: &'a HashMap<i64, i64>,
    pub amend_by_sha: &'a HashMap<String, crate::daemon::amend::AmendInfo>,
    pub feedback_target_by_id: &'a HashMap<i64, (String, String)>,
}

/// Render one timeline row in the initial-page context.
fn timeline_entry_full(ev: &Event, ctx: &TimelineRenderCtx) -> Markup {
    render_entry(ev, ctx, false)
}

/// Shared row renderer. `live=true` adds the `live` class and the
/// `hx-swap-oob="afterbegin:#timeline-feed"` attribute so an htmx SSE
/// fragment prepends to the timeline.
fn render_entry(ev: &Event, ctx: &TimelineRenderCtx, live: bool) -> Markup {
    let kind_class = timeline_entry_class(&ev.kind);
    let class_attr = if live {
        format!("{kind_class} live")
    } else {
        kind_class.to_string()
    };
    let oob: Option<&str> = live.then_some("afterbegin:#timeline-feed");
    let actions = timeline_entry_actions(ev, ctx);
    let title = timeline_entry_title(ev, ctx);
    let preview = timeline_entry_preview(ev);
    html! {
        article id={ "event-" (ev.id) } class=(class_attr) hx-swap-oob=[oob] {
            div.entry-title { (title) }
            div.entry-meta {
                span.actor { (ev.actor) }
                " · " span.relative title=(absolute_time(ev.ts)) { (relative_time(Some(ev.ts))) }
            }
            @if let Some(p) = preview {
                div.entry-preview { (p) }
            }
            @if let Some(actions) = actions {
                div.entry-actions { (actions) }
            }
        }
    }
}

fn timeline_entry_title(ev: &Event, ctx: &TimelineRenderCtx) -> Markup {
    let payload: serde_json::Value =
        serde_json::from_str(&ev.payload).unwrap_or(serde_json::Value::Null);
    match ev.kind.as_str() {
        "plan_revision_created" => {
            let rev_id = ev.target_id.as_deref().and_then(|s| s.parse::<i64>().ok());
            let n = rev_id.and_then(|id| ctx.plan_rev_number_by_id.get(&id).copied());
            html! { "Plan revision " @if let Some(n) = n { "#" (n) } @else { "—" } }
        }
        "impl_revision_created" => {
            let sha = ev.target_id.as_deref().unwrap_or("");
            let is_amend = matches!(
                ctx.amend_by_sha.get(sha),
                Some(crate::daemon::amend::AmendInfo::Amend { .. })
            );
            html! {
                "Commit " code.mono { (short(sha)) }
                @if is_amend { " " span.kind-badge.amend { "amend" } }
            }
        }
        "head_reset_to_known_sha" => {
            let sha = ev.target_id.as_deref().unwrap_or("");
            html! { "HEAD reset to " code.mono { (short(sha)) } }
        }
        "state_transition" => {
            let from = payload.get("from").and_then(|v| v.as_str()).unwrap_or("");
            let to = payload.get("to").and_then(|v| v.as_str()).unwrap_or("");
            html! { "State: " (from) " → " (to) }
        }
        "feedback_added" => html! { "Feedback added" },
        "feedback_updated" => html! { "Feedback updated" },
        "agent_joined" => {
            let a = ev.actor.clone();
            html! { (a) " joined" }
        }
        "dirty_worktree_warning" => {
            let sha = ev.target_id.as_deref().unwrap_or("");
            html! { "Dirty worktree at " code.mono { (short(sha)) } }
        }
        "plan_file_missing" => html! { "Plan file missing" },
        other => {
            let o = other.to_string();
            html! { (o) }
        }
    }
}

fn timeline_entry_preview(ev: &Event) -> Option<Markup> {
    // For impl commits, surface the first line of commit_message. We
    // don't have direct access to it from `Event`; the payload only
    // carries metadata. Surfacing this would require either a join in
    // SessionDetail's build or an embedded line in events.payload. Skip
    // for v1 — the commit detail page shows the full message on click.
    let payload: serde_json::Value =
        serde_json::from_str(&ev.payload).unwrap_or(serde_json::Value::Null);
    match ev.kind.as_str() {
        // Commit message inline preview: a small follow-up. Left as None
        // for now to keep ev → row pure.
        "impl_revision_created" => None,
        "feedback_added" | "feedback_updated" => {
            // `prior_body` lives on the updated event only; the new body
            // is in the structural feedback table. The user clicks
            // through to read the full body on the artifact page.
            None
        }
        _ => {
            let _ = payload;
            None
        }
    }
}

fn timeline_entry_actions(ev: &Event, ctx: &TimelineRenderCtx) -> Option<Markup> {
    use crate::daemon::amend::AmendInfo;
    let session_id = ctx.session_id;
    match ev.kind.as_str() {
        "plan_revision_created" => {
            let rev_id = ev.target_id.as_deref().and_then(|s| s.parse::<i64>().ok());
            let rev_id = rev_id?;
            let n = ctx.plan_rev_number_by_id.get(&rev_id).copied().unwrap_or(0);
            Some(html! {
                a.pill href={ "/sessions/" (session_id) "/plan_revisions/" (rev_id) } { "View body" }
                @if n > 1 {
                    a.pill href={ "/sessions/" (session_id) "/plan_revisions/" (rev_id) "/diff" } { "Diff to #" (n - 1) }
                }
            })
        }
        "impl_revision_created" => {
            let sha = ev.target_id.as_deref().unwrap_or("");
            match ctx.amend_by_sha.get(sha) {
                Some(AmendInfo::Amend {
                    prev_sha,
                    amend_base_parent_sha,
                }) => Some(html! {
                    a.pill href={ "/sessions/" (session_id) "/commits/" (sha) "?vs=" (prev_sha) } {
                        "Diff vs previous amend"
                    }
                    @if let Some(base) = amend_base_parent_sha.as_deref() {
                        a.pill href={ "/sessions/" (session_id) "/commits/" (sha) "?vs=" (base) } {
                            "Full diff since parent"
                        }
                    } @else {
                        a.pill href={ "/sessions/" (session_id) "/commits/" (sha) } { "Diff parent..commit" }
                    }
                }),
                _ => Some(html! {
                    a.pill href={ "/sessions/" (session_id) "/commits/" (sha) } { "Diff parent..commit" }
                }),
            }
        }
        "head_reset_to_known_sha" => {
            let sha = ev.target_id.as_deref().unwrap_or("");
            Some(
                html! { a.pill href={ "/sessions/" (session_id) "/commits/" (sha) } { "Diff parent..commit" } },
            )
        }
        "feedback_added" | "feedback_updated" => {
            let payload: serde_json::Value =
                serde_json::from_str(&ev.payload).unwrap_or(serde_json::Value::Null);
            let fid = payload.get("feedback_id").and_then(|v| v.as_i64());
            let fid = fid?;
            let (kind, target_id) = ctx.feedback_target_by_id.get(&fid)?;
            if kind == "plan_revision" {
                let rev_id: i64 = target_id.parse().unwrap_or(0);
                let n = ctx.plan_rev_number_by_id.get(&rev_id).copied().unwrap_or(0);
                Some(html! {
                    a.pill href={ "/sessions/" (session_id) "/plan_revisions/" (rev_id) "#feedback-" (fid) } {
                        "Open in plan revision #" (n)
                    }
                })
            } else {
                Some(html! {
                    a.pill href={ "/sessions/" (session_id) "/commits/" (target_id) "#feedback-" (fid) } {
                        "Open in commit " code.mono { (short(target_id)) }
                    }
                })
            }
        }
        _ => None,
    }
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
  const FEED_SEL = '#timeline-feed';
  const REDUCED = window.matchMedia('(prefers-reduced-motion: reduce)').matches;
  if (REDUCED) return;
  let snapshot = new Map();
  function snap() {
    snapshot.clear();
    const feed = document.querySelector(FEED_SEL);
    if (!feed) return;
    for (const el of feed.children) {
      if (!el.id) continue;
      snapshot.set(el.id, el.getBoundingClientRect().top);
    }
  }
  function play() {
    const feed = document.querySelector(FEED_SEL);
    if (!feed) return;
    for (const el of feed.children) {
      if (!el.id || !snapshot.has(el.id)) continue;
      const prev = snapshot.get(el.id);
      const next = el.getBoundingClientRect().top;
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
    snapshot.clear();
  }
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
    layout_wide(
        &format!("Trinity — commit {}", short_sha),
        html! {
            (detail_header(&view.session_id, &format!("Commit {short_sha}"), html! {
                span.base-chip { (base_chip) }
            }))
            section.detail-meta {
                "Full SHA: " code.mono { (view.commit.commit_sha) }
                " · " (relative_time(Some(view.commit.created_at)))
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
            section.diff-pane.raw-diff { pre { (view.diff) } }
            (feedback_panel(&view.feedback))
        },
    )
}

// ---------- helpers ----------

fn feedback_card(item: &FeedbackItem) -> Markup {
    let updated = item.updated_at != item.created_at;
    html! {
        div.feedback {
            header.feedback-head {
                span.actor { (item.author_label) }
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
        section.inline-feedback {
            h2 { "Feedback" }
            @for fb in feedback {
                article.inline-feedback-item id={ "feedback-" (fb.feedback_id) } {
                    header.feedback-head {
                        span.actor { (fb.author_label) }
                        " · " span.kind-badge { (fb.feedback_kind) }
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
    let mut amend_by_sha: HashMap<String, crate::daemon::amend::AmendInfo> = HashMap::new();
    let mut feedback_target_by_id: HashMap<i64, (String, String)> = HashMap::new();

    match ev.kind.as_str() {
        "plan_revision_created" => {
            if let Some(rev_id) = ev.target_id.as_deref().and_then(|s| s.parse::<i64>().ok())
                && let Ok(Some(rev)) = plan_revisions::fetch(pool, rev_id).await
            {
                plan_rev_number_by_id.insert(rev.id, rev.revision_number);
            }
        }
        "impl_revision_created" | "head_reset_to_known_sha" => {
            if let Some(plan_id) = ev.plan_id
                && let Ok(rows) = implementation_revisions::list_for_plan(pool, plan_id).await
            {
                let infos = crate::daemon::amend::classify(&rows);
                for (row, info) in rows.iter().zip(infos) {
                    amend_by_sha.insert(row.commit_sha.clone(), info);
                }
            }
        }
        "feedback_added" | "feedback_updated" => {
            let payload: serde_json::Value =
                serde_json::from_str(&ev.payload).unwrap_or(serde_json::Value::Null);
            if let Some(fid) = payload.get("feedback_id").and_then(|v| v.as_i64())
                && let Ok(Some(rec)) = feedback_store::fetch(pool, fid).await
            {
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
                feedback_target_by_id.insert(rec.id, (kind, target));
            }
        }
        _ => {}
    }

    let ctx = TimelineRenderCtx {
        session_id: session_id.as_str(),
        plan_rev_number_by_id: &plan_rev_number_by_id,
        amend_by_sha: &amend_by_sha,
        feedback_target_by_id: &feedback_target_by_id,
    };
    render_entry(ev, &ctx, true)
}

fn timeline_entry_class(kind: &str) -> &'static str {
    match kind {
        "plan_revision_created" => "entry plan-rev",
        "impl_revision_created" => "entry impl-commit",
        "head_reset_to_known_sha" => "entry head-reset",
        "feedback_added" | "feedback_updated" => "entry feedback",
        "state_transition" => "entry state",
        "agent_joined" => "entry meta-event",
        "dirty_worktree_warning" => "entry warning",
        "plan_file_missing" => "entry warning",
        _ => "entry meta-event",
    }
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

const STYLE: &str = r#"
:root {
  --bg: #fafaf7; --bg-alt: #ffffff; --fg: #1f1f1f; --muted: #6c6c6c;
  --line: #d8d8d4; --accent: #2b3a55;
  --planning: #ad6b00; --impl: #1d5fb0; --archived: #999;
  --plan-accent: #4338ca;       /* indigo */
  --commit-accent: #16a34a;     /* green  */
  --feedback-accent: #d97706;   /* amber  */
  --state-accent: #6b7280;      /* gray   */
  --head-reset-accent: #ea580c; /* orange */
  --warning-accent: #b45309;
  --pill-bg: #efefe9;
  --pill-bg-hover: #e4e4dd;
  --entry-shadow: 0 1px 2px rgba(0,0,0,0.04);
  --entry-shadow-hover: 0 4px 12px rgba(0,0,0,0.06);
  --entry-radius: 8px;
  --highlight-fade: rgba(67, 56, 202, 0.10);
}
@media (prefers-color-scheme: dark) {
  :root {
    --bg: #14141a; --bg-alt: #1c1c24; --fg: #e8e8ed; --muted: #9da0aa;
    --line: #2c2c36; --accent: #8aa1ff;
    --pill-bg: #25252e; --pill-bg-hover: #2f2f3a;
    --entry-shadow: 0 1px 2px rgba(0,0,0,0.4);
    --entry-shadow-hover: 0 4px 12px rgba(0,0,0,0.5);
    --highlight-fade: rgba(67, 56, 202, 0.22);
  }
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
header.feedback-head { font-size: 0.85rem; color: var(--muted); margin-bottom: 6px; }
header.feedback-head .actor { font-weight: 600; color: var(--fg); }
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

/* ---------- Session detail v2: flat timeline + sticky head ---------- */
header.session-head { display: flex; align-items: center; justify-content: space-between;
  gap: 16px; padding: 14px 18px; margin: 0 0 18px; background: var(--bg-alt);
  border: 1px solid var(--line); border-radius: 8px; flex-wrap: wrap;
  position: sticky; top: 12px; z-index: 5; box-shadow: var(--entry-shadow); }
header.session-head .session-head-left { display: flex; align-items: center; gap: 10px; flex-wrap: wrap; }
header.session-head .session-head-right { display: flex; align-items: center; gap: 14px; color: var(--muted);
  font-size: 0.88rem; flex-wrap: wrap; }
header.session-head h1.session-id { font-size: 1.15rem; margin: 0; font-weight: 600;
  font-family: ui-monospace, "SF Mono", monospace; }
header.session-head a.back { color: var(--muted); text-decoration: none; font-size: 0.9rem; }
header.session-head a.back:hover { text-decoration: underline; }
header.session-head a.muted-link { color: var(--muted); font-size: 0.85rem; text-decoration: none;
  border-bottom: 1px dotted currentColor; }
.head-meta { font-size: 0.85rem; }

section.watched-artifacts { margin: 0 0 18px; padding: 12px 18px; background: var(--bg-alt);
  border: 1px solid var(--line); border-radius: 8px; }
section.watched-artifacts h2 { font-size: 0.95rem; margin: 0 0 8px; color: var(--muted); font-weight: 600; }
section.watched-artifacts h3 { font-size: 0.85rem; margin: 12px 0 6px; color: var(--muted); }

section.timeline-section h2 { font-size: 1rem; margin: 12px 0 10px; color: var(--muted); font-weight: 600;
  text-transform: uppercase; letter-spacing: 0.05em; }
#timeline-feed { display: flex; flex-direction: column; gap: 10px; }

article.entry { position: relative; padding: 12px 14px 12px 18px; background: var(--bg-alt);
  border: 1px solid var(--line); border-radius: var(--entry-radius); box-shadow: var(--entry-shadow);
  transition: box-shadow 180ms ease-out, transform 180ms ease-out;
  animation: timeline-enter 200ms ease-out, timeline-highlight 1500ms ease-out; }
article.entry:hover { box-shadow: var(--entry-shadow-hover); }
article.entry::before { content: ""; position: absolute; left: 6px; top: 14px; bottom: 14px;
  width: 3px; border-radius: 2px; background: var(--state-accent); }
article.entry.plan-rev::before    { background: var(--plan-accent); }
article.entry.impl-commit::before { background: var(--commit-accent); }
article.entry.feedback::before    { background: var(--feedback-accent); }
article.entry.state::before       { background: var(--state-accent); }
article.entry.head-reset::before  { background: var(--head-reset-accent); }
article.entry.warning::before     { background: var(--warning-accent); }
article.entry.meta-event::before  { background: var(--state-accent); opacity: 0.5; }

article.entry .entry-title { font-size: 1rem; font-weight: 600; line-height: 1.3;
  display: flex; align-items: center; gap: 8px; flex-wrap: wrap; }
article.entry .entry-meta { font-size: 0.82rem; color: var(--muted); margin-top: 2px; }
article.entry .entry-meta .actor { font-weight: 600; color: var(--fg); }
article.entry .entry-preview { font-size: 0.9rem; margin-top: 6px; color: var(--fg);
  font-family: ui-monospace, "SF Mono", monospace; font-size: 0.85rem; }
article.entry .entry-actions { margin-top: 10px; display: flex; gap: 6px; flex-wrap: wrap;
  opacity: 0; transition: opacity 120ms ease-out; }
article.entry:hover .entry-actions, article.entry:focus-within .entry-actions { opacity: 1; }
@media (pointer: coarse) {
  article.entry .entry-actions { opacity: 1; }
}

a.pill { display: inline-flex; align-items: center; gap: 4px;
  padding: 3px 10px; border-radius: 999px; font-size: 0.82rem; text-decoration: none;
  background: var(--pill-bg); color: var(--fg); border: 1px solid transparent;
  transition: background 120ms ease-out, border-color 120ms ease-out; }
a.pill:hover { background: var(--pill-bg-hover); }
article.entry.plan-rev    a.pill:hover { border-color: var(--plan-accent); color: var(--plan-accent); }
article.entry.impl-commit a.pill:hover { border-color: var(--commit-accent); color: var(--commit-accent); }
article.entry.feedback    a.pill:hover { border-color: var(--feedback-accent); color: var(--feedback-accent); }
article.entry.head-reset  a.pill:hover { border-color: var(--head-reset-accent); color: var(--head-reset-accent); }

.kind-badge { display: inline-block; padding: 1px 8px; border-radius: 999px;
  font-size: 0.7rem; text-transform: uppercase; letter-spacing: 0.05em;
  background: var(--pill-bg); color: var(--muted); font-weight: 600; }
.kind-badge.amend { background: #fef3c7; color: #92400e; }
@media (prefers-color-scheme: dark) {
  .kind-badge.amend { background: #422a0a; color: #fbbf24; }
}

@keyframes timeline-enter {
  from { opacity: 0; transform: translateY(-6px); }
  to   { opacity: 1; transform: none; }
}
@keyframes timeline-highlight {
  from { background-color: var(--highlight-fade); }
  to   { background-color: var(--bg-alt); }
}
@media (prefers-reduced-motion: reduce) {
  article.entry { animation: none; transition: none; }
  article.entry .entry-actions { opacity: 1; }
}

/* ---------- Detail (diff / revision) wider layout ---------- */
body.wide-layout main { max-width: 1080px; }
header.detail-header { display: flex; align-items: baseline; justify-content: space-between;
  gap: 12px; padding: 12px 16px; margin: 0 0 18px; background: var(--bg-alt);
  border: 1px solid var(--line); border-radius: 8px; flex-wrap: wrap;
  position: sticky; top: 12px; z-index: 5; box-shadow: var(--entry-shadow); }
header.detail-header .detail-crumbs { font-size: 0.88rem; color: var(--muted); }
header.detail-header .detail-crumbs a { color: var(--muted); text-decoration: none; }
header.detail-header .detail-crumbs a:hover { text-decoration: underline; }
header.detail-header .detail-title { font-size: 1.1rem; font-weight: 600; }
header.detail-header .detail-actions { display: flex; gap: 8px; flex-wrap: wrap; align-items: center; }
header.detail-header .detail-actions .base-chip { font-size: 0.85rem; color: var(--muted);
  font-family: ui-monospace, "SF Mono", monospace; padding: 2px 8px; border-radius: 999px;
  background: var(--pill-bg); }
section.detail-meta { font-size: 0.88rem; color: var(--muted); margin: 0 0 14px; }
article.markdown.detail-body { background: var(--bg-alt); border: 1px solid var(--line);
  border-radius: 8px; padding: 22px 26px; }
details.detail-body-fold { margin-top: 12px; }
details.detail-body-fold summary { cursor: pointer; color: var(--muted); font-size: 0.9rem; }
details.commit-msg-fold, details.diff-stat-fold { margin: 10px 0; }
details.commit-msg-fold summary, details.diff-stat-fold summary { cursor: pointer; color: var(--muted);
  font-size: 0.88rem; font-weight: 600; padding: 4px 0; }

section.diff-pane { background: var(--bg-alt); border: 1px solid var(--line);
  border-radius: 8px; padding: 8px 0; overflow-x: auto; font-family: ui-monospace, "SF Mono", monospace;
  font-size: 0.83rem; }
section.diff-pane .diff-line { white-space: pre; padding: 0 16px; }
section.diff-pane .diff-line.ins { background: rgba(22, 163, 74, 0.10); color: #166534; }
section.diff-pane .diff-line.del { background: rgba(220, 38, 38, 0.10); color: #991b1b; }
section.diff-pane .diff-line.ctx { color: var(--fg); }
@media (prefers-color-scheme: dark) {
  section.diff-pane .diff-line.ins { background: rgba(22, 163, 74, 0.20); color: #86efac; }
  section.diff-pane .diff-line.del { background: rgba(220, 38, 38, 0.20); color: #fca5a5; }
}
section.diff-pane.raw-diff pre { margin: 0; padding: 8px 16px; white-space: pre; }

section.inline-feedback { margin-top: 28px; }
section.inline-feedback h2 { font-size: 1rem; margin: 0 0 10px; color: var(--muted);
  text-transform: uppercase; letter-spacing: 0.05em; font-weight: 600; }
article.inline-feedback-item { padding: 12px 14px; background: var(--bg-alt);
  border: 1px solid var(--line); border-radius: 6px; margin-bottom: 8px; }
article.inline-feedback-item .anchor { color: var(--muted); text-decoration: none; font-size: 0.78rem;
  font-family: ui-monospace, "SF Mono", monospace; }
article.inline-feedback-item .feedback-file-path { margin-top: 8px; font-size: 0.78rem; color: var(--muted); }
article.inline-feedback-item:target { border-color: var(--feedback-accent);
  box-shadow: 0 0 0 3px rgba(217, 119, 6, 0.18); }
.head-tag { background: var(--commit-accent); color: white; padding: 1px 6px; border-radius: 4px;
  font-size: 0.7rem; font-weight: 600; letter-spacing: 0.05em; }

article.entry.live { /* animation applied via class on initial render too — that's fine */ }
"#;
