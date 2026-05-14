//! Minimal HTML rendering for the homepage + session detail page.
//!
//! Renders the in-memory state from `Runtime` via inline format strings.
//! No templating engine — strings only. Intentionally bare-bones.

use serde_json::Value;

/// JS snippet shared by the home and session pages. Subscribes to the
/// SSE endpoint, plays a short chime on each event, and debounces page
/// reloads so chips/banners refresh without manual reload.
const LIVE_SCRIPT: &str = r#"
<script>
(function () {
  const url = window.__trinity_sse || '/events';
  let last = 0;
  let reloadTimer = null;
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
  function scheduleReload() {
    if (reloadTimer) clearTimeout(reloadTimer);
    reloadTimer = setTimeout(() => { window.location.reload(); }, 500);
  }
  function connect() {
    const es = new EventSource(url);
    es.onmessage = (ev) => {
      const now = Date.now();
      if (now - last > 300) {
        chime();
        last = now;
      }
      scheduleReload();
    };
    es.onerror = () => {
      es.close();
      setTimeout(connect, 2000);
    };
  }
  connect();
  // Mute toggle on Cmd/Ctrl+M for the impatient.
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

    let impl_commits_html = render_commit_list(
        ctx["implementation_commits"].as_array(),
        id,
        "commit",
        "No implementation commits yet.",
        latest_impl,
    );
    let plan_revisions_html = render_commit_list(
        ctx["plan_revisions"].as_array(),
        id,
        "plan",
        "No plan revisions found.",
        latest_plan,
    );

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
         ul.commits{{list-style:none;padding:0}}\
         ul.commits li{{padding:.25em 0;border-bottom:1px solid #f3f4f6}}\
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
         <h2>Plan revisions</h2>{plan_revisions_html}\
         <h2>Implementation commits</h2>{impl_commits_html}\
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

/// Render a clickable list of commit shas. `route` is `"plan"` or `"commit"`
/// (used in the URL path). Empty list falls back to `none_text`. If the
/// list is empty but `latest_fallback` is non-empty, fall back to a
/// single-entry list with the fallback (for backward compat with older
/// callers that only had `latest_*` data).
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
