//! Minimal HTML rendering for the homepage + session detail page.
//!
//! Renders the in-memory state from `Runtime` via inline format strings.
//! No templating engine — strings only. Intentionally bare-bones.

use serde_json::Value;

/// JS snippet shared by the home and session pages. Subscribes to the
/// SSE endpoint and plays a short chime on each event. Shows a manual
/// "Refresh to see new activity" banner — no auto-reload, since that
/// fights with reading + scrolling.
const LIVE_SCRIPT: &str = r#"
<script>
(function () {
  const url = window.__trinity_sse || '/events';
  let last = 0;
  let audioCtx = null;
  function chime() {
    if (window.__trinity_muted) return;
    try {
      audioCtx = audioCtx || new (window.AudioContext || window.webkitAudioContext)();
      const t = audioCtx.currentTime;
      const o = audioCtx.createOscillator();
      const g = audioCtx.createGain();
      o.type = 'sine'; o.frequency.setValueAtTime(880, t);
      g.gain.setValueAtTime(0.0001, t);
      g.gain.exponentialRampToValueAtTime(0.10, t + 0.01);
      g.gain.exponentialRampToValueAtTime(0.0001, t + 0.15);
      o.connect(g); g.connect(audioCtx.destination);
      o.start(t); o.stop(t + 0.18);
    } catch (e) {}
  }
  function showRefreshBanner() {
    let b = document.getElementById('refresh-banner');
    if (b) return;
    b = document.createElement('div');
    b.id = 'refresh-banner';
    b.innerHTML = '<span>New activity</span> <button onclick="location.reload()">Refresh</button>';
    b.style.cssText = 'position:fixed;top:1em;right:1em;padding:.5em 1em;background:#1f2937;color:#fff;border-radius:.5em;display:flex;gap:.5em;align-items:center;box-shadow:0 4px 12px rgba(0,0,0,.2);z-index:1000';
    b.querySelector('button').style.cssText = 'background:#3b82f6;border:none;color:#fff;padding:.25em .75em;border-radius:.3em;cursor:pointer;font-weight:600';
    document.body.appendChild(b);
  }
  function connect() {
    const es = new EventSource(url);
    es.onmessage = (ev) => {
      const now = Date.now();
      // Coalesce flurries so the chime doesn't machine-gun.
      if (now - last > 300) {
        chime();
        last = now;
      }
      showRefreshBanner();
    };
    es.onerror = () => {
      es.close();
      setTimeout(connect, 2000);
    };
  }
  connect();
  document.addEventListener('keydown', (e) => {
    if ((e.metaKey || e.ctrlKey) && e.key === 'm') {
      window.__trinity_muted = !window.__trinity_muted;
      const tag = document.getElementById('mute-indicator');
      if (tag) tag.textContent = window.__trinity_muted ? '🔇' : '🔔';
    }
  });
})();
</script>
"#;

pub fn home_page(sessions: &Value) -> String {
    let arr = sessions.as_array().cloned().unwrap_or_default();
    let mut rows = String::new();
    for s in &arr {
        let id = s["id"].as_str().unwrap_or("?");
        let phase = s["phase"].as_str().unwrap_or("?");
        let status = s["plan_worktree_status"].as_str().unwrap_or("?");
        let role = s["waiting_on"]["role"].as_str().unwrap_or("?");
        let desc = s["waiting_on"]["description"]
            .as_str()
            .unwrap_or("");
        rows.push_str(&format!(
            "<tr><td><a href=\"/sessions/{id}\">{id}</a></td>\
             <td>{phase}</td><td>{status}</td>\
             <td><span class=\"waiting waiting-{role}\">{role}</span></td>\
             <td>{desc}</td></tr>"
        ));
    }
    format!(
        "<!doctype html><html><head><title>Trinity</title>\
         <style>\
         body{{font-family:system-ui,sans-serif;max-width:980px;margin:2em auto;padding:0 1em}}\
         table{{border-collapse:collapse;width:100%}}\
         th,td{{padding:.5em;border-bottom:1px solid #eee;text-align:left;vertical-align:top}}\
         .waiting{{padding:.1em .5em;border-radius:.5em;font-size:.85em}}\
         .waiting-master{{background:#fde68a;color:#92400e}}\
         .waiting-reviewers{{background:#bfdbfe;color:#1e40af}}\
         .waiting-none{{background:#d1d5db;color:#374151}}\
         #mute-indicator{{float:right;font-size:1.2em;opacity:.6}}\
         </style></head><body>\
         <span id=\"mute-indicator\">🔔</span>\
         <h1>Trinity sessions</h1>\
         <table><thead><tr><th>Session</th><th>Phase</th><th>Worktree</th>\
         <th>Waiting on</th><th>Description</th></tr></thead>\
         <tbody>{rows}</tbody></table>\
         {LIVE_SCRIPT}\
         </body></html>"
    )
}

pub fn session_page(ctx: &Value) -> String {
    let id = ctx["session_id"].as_str().unwrap_or("?");
    let phase = ctx["phase"].as_str().unwrap_or("?");
    let status = ctx["plan_worktree_status"].as_str().unwrap_or("?");
    let role = ctx["waiting_on"]["role"].as_str().unwrap_or("?");
    let reason = ctx["waiting_on"]["reason"].as_str().unwrap_or("?");
    let desc = ctx["waiting_on"]["description"].as_str().unwrap_or("");
    let plan_path = ctx["plan_path"].as_str().unwrap_or("");

    // Build plan-revisions list from pr_hint (when present, in implementing
    // phase) or just show the latest. For planning, list latest only.
    let latest_plan = ctx["latest_plan_revision"]["commit_sha"]
        .as_str()
        .unwrap_or("—");
    let latest_impl = ctx["latest_implementation_revision"]["commit_sha"]
        .as_str()
        .unwrap_or("—");

    let timeline_html = render_timeline(ctx["timeline"].as_array(), id);
    let _ = (latest_plan, latest_impl); // legacy hints, not displayed

    let pr_hint_html = if let Some(hint) = ctx.get("pr_hint").filter(|v| !v.is_null()) {
        let options = hint["options"].as_array().cloned().unwrap_or_default();
        let mut s = String::new();
        s.push_str("<h2>PR squash hint</h2><ul class=\"options\">");
        for opt in options {
            let name = opt["name"].as_str().unwrap_or("?");
            let command = opt["command"].as_str().unwrap_or("?");
            s.push_str(&format!(
                "<li><strong>{name}:</strong> <code>{command}</code></li>"
            ));
        }
        s.push_str("</ul>");
        s
    } else {
        String::new()
    };

    format!(
        "<!doctype html><html><head><title>{id} — Trinity</title>\
         <style>\
         body{{font-family:system-ui,sans-serif;max-width:980px;margin:2em auto;padding:0 1em;color:#111}}\
         .banner{{padding:1em;border-radius:.5em;margin:1em 0}}\
         .banner.master{{background:#fde68a;color:#92400e}}\
         .banner.reviewers{{background:#bfdbfe;color:#1e40af}}\
         .banner.none{{background:#d1d5db;color:#374151}}\
         dl{{display:grid;grid-template-columns:max-content 1fr;gap:.25em 1em}}\
         dt{{font-weight:600}}\
         a{{color:#1d4ed8;text-decoration:none}}\
         a:hover{{text-decoration:underline}}\
         code{{background:#f3f4f6;padding:.1em .3em;border-radius:.25em;font-size:.9em}}\
         ul.commits,ul.feedback,ol.timeline{{list-style:none;padding:0}}\
         ul.commits li,ul.feedback li{{padding:.25em 0;border-bottom:1px solid #f3f4f6}}\
         ol.timeline{{border-left:2px solid #e5e7eb;margin-left:.5em;padding-left:1em}}\
         ol.timeline li{{position:relative;padding:.5em .25em;margin-left:.5em}}\
         ol.timeline li::before{{content:'';position:absolute;left:-1.6em;top:1em;\
         width:.6em;height:.6em;border-radius:50%;background:#9ca3af}}\
         li.tl-plan::before{{background:#3b82f6}}\
         li.tl-impl::before{{background:#10b981}}\
         li.tl-mixed::before{{background:#8b5cf6}}\
         li.tl-done::before{{background:#6b7280}}\
         li.tl-review::before{{background:#f59e0b;width:.4em;height:.4em;left:-1.5em;top:1.1em}}\
         li.tl-held::before{{background:#e5e7eb;border:2px solid #f59e0b;left:-1.7em;top:1em}}\
         .tl-label{{font-weight:600;margin-right:.5em}}\
         .tl-phase{{color:#6b7280;font-size:.85em}}\
         .verdict-approve{{display:inline-block;padding:.1em .5em;border-radius:.4em;\
         background:#d1fae5;color:#065f46;font-weight:600;font-size:.85em}}\
         .verdict-request{{display:inline-block;padding:.1em .5em;border-radius:.4em;\
         background:#fee2e2;color:#991b1b;font-weight:600;font-size:.85em}}\
         .verdict-unmarked{{display:inline-block;padding:.1em .5em;border-radius:.4em;\
         background:#e5e7eb;color:#374151;font-style:italic;font-size:.85em}}\
         .none{{color:#6b7280;font-style:italic}}\
         h2{{margin-top:2em}}\
         </style></head><body>\
         <p><a href=\"/\">&larr; All sessions</a></p>\
         <h1>{id}</h1>\
         <div class=\"banner {role}\">\
         <strong>{role}</strong>: {desc} <em>({reason})</em>\
         </div>\
         <dl>\
         <dt>Phase</dt><dd>{phase}</dd>\
         <dt>Plan path</dt><dd><code>{plan_path}</code></dd>\
         <dt>Worktree</dt><dd>{status}</dd>\
         </dl>\
         <h2>Timeline</h2>{timeline_html}\
         {pr_hint_html}\
         <script>window.__trinity_sse = '/sessions/{id}/events';</script>\
         {LIVE_SCRIPT}\
         </body></html>"
    )
}

pub fn plan_revision_page(session_id: &str, sha: &str, body: &str) -> String {
    let escaped = html_escape(body);
    let short = &sha[..7.min(sha.len())];
    format!(
        "<!doctype html><html><head><title>{session_id}@{short} — Trinity</title>\
         <style>\
         body{{font-family:system-ui,sans-serif;max-width:980px;margin:2em auto;padding:0 1em}}\
         pre{{background:#f3f4f6;padding:1em;border-radius:.5em;overflow-x:auto;\
         font-family:ui-monospace,SFMono-Regular,Menlo,monospace;font-size:.9em;line-height:1.5}}\
         a{{color:#1d4ed8;text-decoration:none}}\
         a:hover{{text-decoration:underline}}\
         </style></head><body>\
         <p><a href=\"/sessions/{session_id}\">&larr; {session_id}</a></p>\
         <h1>Plan @ <code>{short}</code></h1>\
         <pre>{escaped}</pre>\
         </body></html>"
    )
}

pub fn commit_diff_page(session_id: &str, sha: &str, patch: &str) -> String {
    let escaped = html_escape(patch);
    let short = &sha[..7.min(sha.len())];
    format!(
        "<!doctype html><html><head><title>{session_id} commit {short} — Trinity</title>\
         <style>\
         body{{font-family:system-ui,sans-serif;max-width:980px;margin:2em auto;padding:0 1em}}\
         pre{{background:#0f172a;color:#e2e8f0;padding:1em;border-radius:.5em;\
         overflow-x:auto;font-family:ui-monospace,SFMono-Regular,Menlo,monospace;\
         font-size:.85em;line-height:1.5}}\
         a{{color:#1d4ed8;text-decoration:none}}\
         a:hover{{text-decoration:underline}}\
         </style></head><body>\
         <p><a href=\"/sessions/{session_id}\">&larr; {session_id}</a></p>\
         <h1>Commit <code>{short}</code></h1>\
         <pre>{escaped}</pre>\
         </body></html>"
    )
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Render the unified per-session timeline. Each event is one `<li>`
/// in an ordered list. Commits are clickable to their plan-revision or
/// commit-diff view; reviews are indented under their target commit
/// and carry the verdict chip + author name.
fn render_timeline(events: Option<&Vec<Value>>, session_id: &str) -> String {
    let events = events.map(|v| v.as_slice()).unwrap_or(&[]);
    if events.is_empty() {
        return "<p class=\"none\">No activity yet.</p>".to_string();
    }
    let mut s = String::from("<ol class=\"timeline\">");
    for ev in events {
        let kind = ev["kind"].as_str().unwrap_or("");
        match kind {
            "commit_plan" | "commit_impl" | "commit_mixed" | "commit_other" => {
                let sha = ev["sha"].as_str().unwrap_or("");
                let short = &sha[..7.min(sha.len())];
                let plan_touch = ev["plan_touch"].as_str();
                let has_code = ev["has_code_changes"].as_bool().unwrap_or(false);
                let (label, css) = match (plan_touch, has_code) {
                    (Some("intro"), false) => ("Plan created", "tl-plan"),
                    (Some("intro"), true) => ("Plan created + impl", "tl-mixed"),
                    (Some("revision"), false) => ("Plan revised", "tl-plan"),
                    (Some("revision"), true) => ("Plan revised + impl", "tl-mixed"),
                    (Some("done_move"), _) => ("Moved to done/", "tl-done"),
                    (None, true) => ("Implementation", "tl-impl"),
                    _ => ("Commit", "tl-other"),
                };
                // For done_move, link to the commit diff; for plan-touch
                // (revision/intro), link to the plan body at that sha;
                // for pure impl, link to the commit diff.
                let route = if plan_touch == Some("done_move") {
                    "commit"
                } else if plan_touch.is_some() {
                    "plan"
                } else {
                    "commit"
                };
                s.push_str(&format!(
                    "<li class=\"tl-row {css}\"><span class=\"tl-label\">{label}</span> \
                     <a href=\"/sessions/{session_id}/{route}/{sha}\"><code>{short}</code></a></li>"
                ));
            }
            "review" => {
                let phase = ev["phase"].as_str().unwrap_or("");
                let author = ev["author"].as_str().unwrap_or("?");
                let verdict = ev["verdict"].as_str().unwrap_or("unmarked");
                let target = ev["target"].as_str().unwrap_or("");
                let short = &target[..7.min(target.len())];
                let (verdict_css, verdict_label) = match verdict {
                    "approve" => ("verdict-approve", "APPROVE"),
                    "request_changes" => ("verdict-request", "REQUEST_CHANGES"),
                    _ => ("verdict-unmarked", "(unmarked)"),
                };
                let route = if phase == "plan" { "plan" } else { "commit" };
                s.push_str(&format!(
                    "<li class=\"tl-row tl-review\"><span class=\"{verdict_css}\">{verdict_label}</span> \
                     <span class=\"tl-phase\">({phase})</span> from <strong>{author}</strong> on \
                     <a href=\"/sessions/{session_id}/{route}/{target}\"><code>{short}</code></a></li>"
                ));
            }
            "held_feedback" => {
                let author = ev["author"].as_str().unwrap_or("?");
                let reason = ev["reason"].as_str().unwrap_or("?");
                s.push_str(&format!(
                    "<li class=\"tl-row tl-held\"><span class=\"verdict-unmarked\">HELD</span> \
                     from <strong>{author}</strong> <em>({reason})</em></li>"
                ));
            }
            _ => {}
        }
    }
    s.push_str("</ol>");
    s
}

/// Render the per-phase feedback list. Each entry: verdict chip + author
/// + (clickable) target SHA. Empty list → "No feedback yet."
#[allow(dead_code)]
fn render_feedback_list(
    entries: Option<&Vec<Value>>,
    phase: &str,
    session_id: &str,
) -> String {
    let entries = entries.map(|v| v.as_slice()).unwrap_or(&[]);
    if entries.is_empty() {
        return format!("<p class=\"none\">No {phase} feedback yet.</p>");
    }
    let route = match phase {
        "plan" => "plan",
        _ => "commit",
    };
    let mut s = String::from("<ul class=\"feedback\">");
    for e in entries {
        let verdict = e["verdict"].as_str().unwrap_or("unmarked");
        let author = e["author"].as_str().unwrap_or("?");
        let target = e["target_sha"].as_str().unwrap_or("");
        let short = &target[..7.min(target.len())];
        let css = match verdict {
            "approve" => "verdict-approve",
            "request_changes" => "verdict-request",
            _ => "verdict-unmarked",
        };
        let pretty = match verdict {
            "approve" => "APPROVE",
            "request_changes" => "REQUEST_CHANGES",
            _ => "(unmarked)",
        };
        s.push_str(&format!(
            "<li><span class=\"{css}\">{pretty}</span> from <strong>{author}</strong> on \
             <a href=\"/sessions/{session_id}/{route}/{target}\"><code>{short}</code></a></li>"
        ));
    }
    s.push_str("</ul>");
    s
}

/// Render a clickable list of commit shas. `route` is `"plan"` or `"commit"`
/// (used in the URL path). Empty list falls back to `none_text`. If the
/// list is empty but `latest_fallback` is non-empty, fall back to a
/// single-entry list with the fallback (for backward compat with older
/// callers that only had `latest_*` data).
#[allow(dead_code)]
fn render_commit_list(
    commits: Option<&Vec<Value>>,
    session_id: &str,
    route: &str,
    none_text: &str,
    latest_fallback: &str,
) -> String {
    let shas: Vec<&str> = commits
        .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    if !shas.is_empty() {
        let mut s = String::from("<ul class=\"commits\">");
        for (i, sha) in shas.iter().rev().enumerate() {
            let short = &sha[..7.min(sha.len())];
            let marker = if i == 0 { " <em>(latest)</em>" } else { "" };
            s.push_str(&format!(
                "<li><a href=\"/sessions/{session_id}/{route}/{sha}\"><code>{short}</code></a>{marker}</li>"
            ));
        }
        s.push_str("</ul>");
        s
    } else if latest_fallback != "—" {
        let short = &latest_fallback[..7.min(latest_fallback.len())];
        format!(
            "<ul class=\"commits\"><li><a href=\"/sessions/{session_id}/{route}/{latest_fallback}\"><code>{short}</code></a></li></ul>"
        )
    } else {
        format!("<p class=\"none\">{none_text}</p>")
    }
}
