pub const STYLE: &str = r#"
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
  --code-bg: #f3f3ee;
  --code-fg: #1f1f1f;
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
    --code-bg: #25252e;
    --code-fg: #e8e8ed;
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
table.sessions tbody tr { font-size: 0.85rem; }
table.sessions tbody td { padding: 6px 12px; vertical-align: middle; }
table.sessions tbody tr.session-row.live { animation: timeline-highlight 1500ms ease-out; }
table.sessions .plan-cell { font-family: ui-monospace, "SF Mono", monospace; font-size: 0.85rem; color: var(--fg); }
table.sessions .row-actions { width: 36px; text-align: right; padding-right: 10px; }
table.sessions th.row-actions-th { width: 36px; }
.row-delete-form { display: inline-flex; margin: 0; padding: 0; }
.row-action-button { width: 28px; height: 28px; padding: 0; display: inline-flex; align-items: center; justify-content: center;
  background: transparent; border: 1px solid transparent; border-radius: 4px; color: var(--muted); cursor: pointer; }
.row-action-button:hover { color: var(--fg); background: var(--pill-bg-hover); border-color: var(--line); }
.row-action-button.danger:hover { color: #b91c1c; border-color: rgba(185, 28, 28, 0.4); }
.row-action-button.success:hover { color: #166534; border-color: rgba(22, 101, 52, 0.4); }
.row-finish-form { display: inline-flex; margin: 0 4px 0 0; padding: 0; }
.row-action-button svg { width: 14px; height: 14px; fill: none; stroke: currentColor; stroke-width: 1.75;
  stroke-linecap: round; stroke-linejoin: round; }
.status-chip { display: inline-block; padding: 2px 10px; border-radius: 999px; font-size: 0.75rem;
  font-weight: 600; letter-spacing: 0.02em; }
.status-chip.planning    { background: #fff1d6; color: var(--planning); }
.status-chip.implementing{ background: #d6e5f7; color: var(--impl); }
.status-chip.finished    { background: #d6f0db; color: #166534; }
.status-chip.ready       { background: #dcfce7; color: #166534; }
.status-chip.blocked     { background: #fee2e2; color: #991b1b; }
.status-chip.muted       { background: #eee;    color: var(--muted); }
@media (prefers-color-scheme: dark) {
  .status-chip.planning    { background: rgba(255, 193, 7, 0.18); color: #fbbf24; }
  .status-chip.implementing{ background: rgba(67, 134, 240, 0.20); color: #93c5fd; }
  .status-chip.finished    { background: rgba(22, 163, 74, 0.20); color: #86efac; }
  .status-chip.ready       { background: rgba(22, 163, 74, 0.20); color: #86efac; }
  .status-chip.blocked     { background: rgba(220, 38, 38, 0.24); color: #fca5a5; }
  .status-chip.muted       { background: rgba(255,255,255,0.06); color: var(--muted); }
}
.home-header { display: flex; align-items: center; justify-content: space-between; gap: 12px; margin-bottom: 12px; }
.home-header h1 { margin: 0; }
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
article.markdown pre { background: var(--code-bg); color: var(--code-fg); padding: 10px 12px; border-radius: 4px; overflow-x: auto; font-size: 0.85rem; }
article.markdown code { background: var(--code-bg); color: var(--code-fg); padding: 1px 4px; border-radius: 3px; font-size: 0.85rem; font-family: ui-monospace, "SF Mono", monospace; }
article.markdown pre code { background: none; color: inherit; padding: 0; }
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
section.watched-artifacts summary { cursor: pointer; display: flex; gap: 8px; align-items: center; flex-wrap: wrap;
  color: var(--muted); font-weight: 600; }
.summary-separator { color: var(--line); font-weight: 400; }
section.watched-artifacts h3 { font-size: 0.85rem; margin: 12px 0 6px; color: var(--muted); }
section.watched-artifacts dl { display: grid; grid-template-columns: 170px 1fr; gap: 4px 14px; margin: 12px 0; }
section.watched-artifacts dt { color: var(--muted); font-size: 0.78rem; text-transform: uppercase; letter-spacing: 0.04em; }
section.watched-artifacts dd { margin: 0; }
.artifact-chip { display: inline-flex; align-items: center; min-height: 22px; padding: 1px 8px; border-radius: 999px;
  background: var(--pill-bg); color: var(--muted); font-size: 0.74rem; text-transform: uppercase; letter-spacing: 0.04em; }
.artifact-chip.ok { color: #166534; background: rgba(22, 163, 74, 0.10); }
.artifact-chip.warn { color: #92400e; background: #fef3c7; }

.active-plan-preview { margin: 0 0 18px; }
.active-plan-preview details { background: var(--bg-alt); border: 1px solid var(--line); border-radius: 8px; padding: 10px 14px; }
.active-plan-preview summary, .active-plan-head { display: flex; justify-content: space-between; gap: 12px; align-items: center;
  color: var(--muted); font-weight: 600; }
.active-plan-preview summary { cursor: pointer; }
.active-plan-preview article.markdown { margin-top: 10px; border: 0; padding: 10px 0 0; background: transparent; }
.active-plan-preview .active-plan > summary { list-style: none; }
.active-plan-preview .active-plan > summary::-webkit-details-marker { display: none; }
.active-plan-preview .active-plan > summary::after { content: " · expand"; font-size: 0.82rem; color: var(--muted); font-weight: 400; }
.active-plan-preview .active-plan[open] > summary::after { content: " · collapse"; }
.active-plan-preview .active-plan > article.markdown { display: block; max-height: 26rem; overflow: hidden;
  -webkit-mask-image: linear-gradient(180deg, #000 calc(100% - 64px), transparent);
  mask-image: linear-gradient(180deg, #000 calc(100% - 64px), transparent); }
.active-plan-preview .active-plan[open] > article.markdown { max-height: none; overflow: visible;
  -webkit-mask-image: none; mask-image: none; }
.plan-preview-meta { display: inline-flex; gap: 8px; align-items: center; font-weight: 400; }

section.timeline-section h2 { font-size: 1rem; margin: 12px 0 10px; color: var(--muted); font-weight: 600;
  text-transform: uppercase; letter-spacing: 0.05em; }
.section-title-row { display: flex; align-items: center; justify-content: space-between; gap: 12px; }
.timeline-feed { display: flex; flex-direction: column; gap: 10px; }
.home-activity .timeline-wrap { max-height: 70vh; overflow-y: auto; padding-right: 2px; }

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
article.entry.needs-attention { border-color: rgba(217, 119, 6, 0.42); }

article.entry .entry-title { font-size: 1rem; font-weight: 600; line-height: 1.3;
  display: flex; align-items: center; gap: 8px; flex-wrap: wrap; }
article.entry .entry-top { display: flex; justify-content: space-between; gap: 12px; align-items: flex-start; }
article.entry .entry-title-group { min-width: 0; }
article.entry .entry-session-prefix { font-size: 0.78rem; line-height: 1.2; margin-bottom: 3px; }
article.entry .entry-session-prefix a { color: var(--muted); text-decoration: none; }
article.entry .entry-session-prefix a:hover { text-decoration: underline; }
article.entry .entry-meta { font-size: 0.82rem; color: var(--muted); margin-top: 2px; }
article.entry .entry-meta .actor { font-weight: 600; color: var(--fg); }
article.entry .entry-preview { font-size: 0.9rem; margin-top: 6px; color: var(--fg);
  font-size: 0.88rem; max-width: 86ch; overflow-wrap: anywhere; }
article.entry .entry-actions { display: flex; gap: 6px; flex-wrap: nowrap; flex: 0 0 auto; }

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
.kind-badge.warning-badge, .attention-badge { background: #fef3c7; color: #92400e; }
.kind-badge.verdict.approve { background: #dcfce7; color: #166534; }
.kind-badge.verdict.request-changes { background: #fee2e2; color: #991b1b; }
.kind-badge.verdict.unmarked { background: #f3f4f6; color: #4b5563; }
.kind-badge.review-gate.ready { background: #dcfce7; color: #166534; }
.kind-badge.review-gate.changes_requested { background: #fee2e2; color: #991b1b; }
.kind-badge.review-gate.needs_review { background: #e0f2fe; color: #075985; }
.attention-badge { display: inline-block; padding: 1px 8px; border-radius: 999px; font-size: 0.7rem;
  text-transform: uppercase; letter-spacing: 0.05em; font-weight: 700; }
.icon-action { min-height: 32px; display: inline-flex; align-items: center; justify-content: center;
  border: 1px solid var(--line); border-radius: 6px; background: var(--pill-bg); color: var(--fg); text-decoration: none; }
.icon-action { gap: 5px; padding: 0 10px; font: inherit; font-size: 0.82rem; font-weight: 600; white-space: nowrap;
  cursor: pointer; }
.icon-action:hover { background: var(--pill-bg-hover); }
.icon-action .action-icon { display: inline-flex; align-items: center; justify-content: center; }
.icon-action svg { width: 14px; height: 14px; fill: none; stroke: currentColor; stroke-width: 1.75;
  stroke-linecap: round; stroke-linejoin: round; }
.review-gates { display: grid; grid-template-columns: repeat(auto-fit, minmax(280px, 1fr)); gap: 10px; margin: 12px 0; }
.review-gate-card { border: 1px solid var(--line); background: var(--bg-alt); border-radius: 8px; padding: 12px; }
.review-gate-main { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; }
.review-gate-phase { font-weight: 700; }
.review-gate-detail { margin-top: 6px; color: var(--muted); font-size: 0.88rem; }
.review-gate-override { color: var(--muted); font-size: 0.82rem; }
.review-gate-actions { margin-top: 10px; display: flex; gap: 8px; flex-wrap: wrap; }
.sr-only { position: absolute; width: 1px; height: 1px; padding: 0; margin: -1px; overflow: hidden;
  clip: rect(0, 0, 0, 0); white-space: nowrap; border: 0; }
@media (prefers-color-scheme: dark) {
  .kind-badge.amend { background: #422a0a; color: #fbbf24; }
  .kind-badge.warning-badge, .attention-badge, .artifact-chip.warn { background: #422a0a; color: #fbbf24; }
  .kind-badge.verdict.approve { background: rgba(22, 163, 74, 0.20); color: #86efac; }
  .kind-badge.verdict.request-changes { background: rgba(220, 38, 38, 0.24); color: #fca5a5; }
  .kind-badge.verdict.unmarked { background: rgba(255,255,255,0.06); color: var(--muted); }
  .kind-badge.review-gate.ready { background: rgba(22, 163, 74, 0.20); color: #86efac; }
  .kind-badge.review-gate.changes_requested { background: rgba(220, 38, 38, 0.24); color: #fca5a5; }
  .kind-badge.review-gate.needs_review { background: rgba(14, 165, 233, 0.20); color: #7dd3fc; }
  .artifact-chip.ok { color: #86efac; background: rgba(22, 163, 74, 0.20); }
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

.file-index { margin: 18px 0 12px; display: grid; gap: 6px; }
.file-index.two-col { grid-template-columns: repeat(2, minmax(0, 1fr)); }
.file-index a { display: flex; justify-content: space-between; gap: 12px; align-items: center;
  padding: 7px 10px; background: var(--bg-alt); border: 1px solid var(--line); border-radius: 6px;
  color: var(--fg); text-decoration: none; font-size: 0.86rem; }
.file-index a:hover { border-color: var(--accent); }
.file-index-path { min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap;
  font-family: ui-monospace, "SF Mono", monospace; }
.file-index-stat, .file-stat, .file-mode { color: var(--muted); font-variant-numeric: tabular-nums; flex: 0 0 auto; }
.structured-diff { display: flex; flex-direction: column; gap: 12px; }
details.file-diff { background: var(--bg-alt); border: 1px solid var(--line); border-radius: 8px; overflow: hidden; }
details.file-diff summary { cursor: pointer; display: flex; align-items: center; gap: 10px;
  padding: 9px 12px; background: #f3f3ee; font-size: 0.86rem; }
.file-diff-path { flex: 1 1 auto; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap;
  font-family: ui-monospace, "SF Mono", monospace; font-weight: 600; }
.file-mode { text-transform: uppercase; letter-spacing: 0.04em; font-size: 0.7rem; }
.diff-table { overflow-x: auto; font-family: ui-monospace, "SF Mono", monospace; font-size: 0.82rem; }
.diff-row, .diff-hunk-header { display: grid; grid-template-columns: 54px 54px minmax(0, 1fr); min-width: 720px; }
.diff-hunk-header { background: rgba(67, 56, 202, 0.08); color: var(--plan-accent); }
.lineno { user-select: none; text-align: right; padding: 0 8px; color: var(--muted); border-right: 1px solid var(--line);
  font-variant-numeric: tabular-nums; }
.diff-content { white-space: pre; padding: 0 12px; }
.diff-marker { display: inline-block; width: 16px; color: var(--muted); user-select: none; }
.diff-row.ins { background: rgba(22, 163, 74, 0.10); color: #166534; }
.diff-row.del { background: rgba(220, 38, 38, 0.10); color: #991b1b; }
.diff-row.ctx { color: var(--fg); }
.diff-row.meta { color: var(--muted); }
.binary-diff { padding: 18px; color: var(--muted); }
@media (prefers-color-scheme: dark) {
  details.file-diff summary { background: #23232b; }
  .diff-hunk-header { background: rgba(138, 161, 255, 0.12); color: #a5b4fc; }
  .diff-row.ins { background: rgba(22, 163, 74, 0.20); color: #86efac; }
  .diff-row.del { background: rgba(220, 38, 38, 0.20); color: #fca5a5; }
}
@media (max-width: 760px) {
  .file-index.two-col { grid-template-columns: 1fr; }
  article.entry .entry-top { flex-direction: column; }
  article.entry .entry-actions { align-self: flex-start; }
  section.watched-artifacts dl { grid-template-columns: 1fr; }
}
@media (max-width: 640px) {
  .icon-action { width: 32px; padding: 0; }
  .icon-action .action-label { position: absolute; width: 1px; height: 1px; padding: 0; margin: -1px;
    overflow: hidden; clip: rect(0, 0, 0, 0); white-space: nowrap; border: 0; }
}

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
