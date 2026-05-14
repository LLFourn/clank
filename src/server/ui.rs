//! Minimal HTML rendering for the homepage + session detail page.
//!
//! Renders the in-memory state from `Runtime` via inline format strings.
//! No templating engine — strings only. Intentionally bare-bones; the
//! old SQL-era UI's polish (chime, charts, polish styling) will be
//! ported in a follow-up.

use serde_json::Value;

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
         </style></head><body>\
         <h1>Trinity sessions</h1>\
         <table><thead><tr><th>Session</th><th>Phase</th><th>Worktree</th>\
         <th>Waiting on</th><th>Description</th></tr></thead>\
         <tbody>{rows}</tbody></table>\
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
    let latest_plan = ctx["latest_plan_revision"]["commit_sha"]
        .as_str()
        .unwrap_or("—");
    let latest_impl = ctx["latest_implementation_revision"]["commit_sha"]
        .as_str()
        .unwrap_or("—");

    format!(
        "<!doctype html><html><head><title>{id} — Trinity</title>\
         <style>\
         body{{font-family:system-ui,sans-serif;max-width:980px;margin:2em auto;padding:0 1em}}\
         .banner{{padding:1em;border-radius:.5em;margin:1em 0}}\
         .banner.master{{background:#fde68a;color:#92400e}}\
         .banner.reviewers{{background:#bfdbfe;color:#1e40af}}\
         .banner.none{{background:#d1d5db;color:#374151}}\
         dl{{display:grid;grid-template-columns:max-content 1fr;gap:.25em 1em}}\
         dt{{font-weight:600}}\
         a{{color:#1d4ed8;text-decoration:none}}\
         a:hover{{text-decoration:underline}}\
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
         <dt>Latest plan revision</dt><dd><code>{latest_plan}</code></dd>\
         <dt>Latest impl commit</dt><dd><code>{latest_impl}</code></dd>\
         </dl>\
         </body></html>"
    )
}
