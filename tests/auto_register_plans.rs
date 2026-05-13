//! Auto-register behaviour for `.trinity/plans/*.md` files. Covers the
//! recovery-vs-live-create distinction (deleted sessions must survive
//! restart even though their plan file stays on disk), basename
//! collision disambiguation across repos, slug validation, and the
//! one-active-per-repo invariant on reactivation.

mod common;

use std::path::Path;

use serde_json::json;

use common::TestApp;

use common::SETTLE;

async fn drop_plan(repo: &Path, name: &str, body: &str) -> std::path::PathBuf {
    let dir = repo.join(".trinity").join("plans");
    tokio::fs::create_dir_all(&dir).await.unwrap();
    let path = dir.join(name);
    tokio::fs::write(&path, body).await.unwrap();
    path
}

async fn delete_session(app: &TestApp, session_id: &str) {
    let resp = app
        .client
        .post(format!("{}/sessions/{session_id}/delete", app.base))
        .header("origin", "http://127.0.0.1")
        .send()
        .await
        .unwrap();
    assert!(
        resp.status().is_success() || resp.status().as_u16() == 303,
        "delete returned {}",
        resp.status()
    );
}

#[tokio::test]
async fn recovery_scan_does_not_reactivate_deleted_sessions() {
    let app = TestApp::spawn().await;

    // Bootstrap the repo by registering a plan; this also makes the
    // repo "known" so recovery's plans-dir scan picks it up on restart.
    let plan_path = drop_plan(&app.repo, "alpha.md", "# alpha\n").await;
    app.call(
        "register_plan_file",
        &app.repo,
        Some("m"),
        json!({"session_id": "alpha", "path": &plan_path}),
    )
    .await
    .unwrap();

    // Delete the session via the same endpoint the homepage trash icon hits.
    delete_session(&app, "alpha").await;

    let archived_before_restart: Option<i64> =
        sqlx::query_scalar("SELECT archived_at FROM sessions WHERE id = 'alpha'")
            .fetch_one(&app.state.pool)
            .await
            .unwrap();
    assert!(
        archived_before_restart.is_some(),
        "session should be archived after delete"
    );

    // Plan file stays on disk by design — confirm we did not touch it.
    assert!(plan_path.exists(), "delete must not remove the plan file");

    // Restart daemon. Recovery scans `.trinity/plans/*.md` for the repo
    // and must NOT reactivate the archived session even though the
    // basename matches a `.md` on disk.
    let app = app.restart().await;

    let archived_after_restart: Option<i64> =
        sqlx::query_scalar("SELECT archived_at FROM sessions WHERE id = 'alpha'")
            .fetch_one(&app.state.pool)
            .await
            .unwrap();
    assert!(
        archived_after_restart.is_some(),
        "deleted session must stay archived across daemon restart"
    );

    let active_plan_id: Option<i64> =
        sqlx::query_scalar("SELECT active_plan_id FROM sessions WHERE id = 'alpha'")
            .fetch_one(&app.state.pool)
            .await
            .unwrap();
    assert!(
        active_plan_id.is_none(),
        "deleted session must have no active plan after restart"
    );
}

#[tokio::test]
async fn invalid_slug_filename_is_skipped_by_recovery() {
    let app = TestApp::spawn().await;
    // Make the repo known so recovery scans its plans dir.
    let primer = drop_plan(&app.repo, "valid.md", "# valid\n").await;
    app.call(
        "register_plan_file",
        &app.repo,
        Some("m"),
        json!({"session_id": "valid", "path": &primer}),
    )
    .await
    .unwrap();

    // Drop a file whose basename isn't a valid slug. The plan rule:
    // skip silently; don't try to munge the name.
    let dir = app.repo.join(".trinity").join("plans");
    let bad = dir.join("has spaces.md");
    tokio::fs::write(&bad, "# bad\n").await.unwrap();

    let app = app.restart().await;
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sessions WHERE plan_file_path LIKE '%has spaces.md'",
    )
    .fetch_one(&app.state.pool)
    .await
    .unwrap();
    assert_eq!(count, 0, "invalid-slug filename must not produce a session");
}

#[tokio::test]
async fn live_drop_into_plans_dir_auto_registers() {
    let app = TestApp::spawn().await;
    // Prime: register one session so the repo is known and the
    // plan-dir watcher is attached.
    let primer = drop_plan(&app.repo, "primer.md", "# primer\n").await;
    app.call(
        "register_plan_file",
        &app.repo,
        Some("m"),
        json!({"session_id": "primer", "path": &primer}),
    )
    .await
    .unwrap();

    // Drop a new file in the live state — the watcher should fire and
    // auto-register it.
    drop_plan(&app.repo, "fresh-arrival.md", "# fresh\n").await;
    tokio::time::sleep(SETTLE).await;

    let exists: Option<String> =
        sqlx::query_scalar("SELECT id FROM sessions WHERE id = 'fresh-arrival'")
            .fetch_optional(&app.state.pool)
            .await
            .unwrap();
    assert_eq!(
        exists.as_deref(),
        Some("fresh-arrival"),
        "live drop should auto-register a session named after the basename"
    );
}

#[tokio::test]
async fn reactivation_drift_scan_replays_unchanged_feedback_files() {
    let app = TestApp::spawn().await;

    let plan_path = drop_plan(&app.repo, "beta.md", "# beta\n").await;
    let r = app
        .call(
            "register_plan_file",
            &app.repo,
            Some("m"),
            json!({"session_id": "beta", "path": &plan_path}),
        )
        .await
        .unwrap();
    let plan_id = r["plan_id"].as_i64().unwrap();
    let rev_id = r["revision_id"].as_i64().unwrap();

    // Drop a plan-feedback file (under the registered session's feedback dir)
    // and let the watcher ingest it normally.
    let plan_fb_dir = app
        .repo
        .join(".trinity")
        .join("feedback")
        .join("beta")
        .join("plan");
    tokio::fs::create_dir_all(&plan_fb_dir).await.unwrap();
    tokio::fs::write(plan_fb_dir.join("rev-a.md"), "the prior review\n")
        .await
        .unwrap();
    tokio::time::sleep(SETTLE).await;

    let original_fb: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM feedback WHERE plan_id = ? AND author_label = 'rev-a'",
    )
    .bind(plan_id)
    .fetch_one(&app.state.pool)
    .await
    .unwrap();
    assert_eq!(original_fb, 1, "feedback should ingest against initial plan");

    // Delete the session, leaving the plan file and feedback files on disk.
    delete_session(&app, "beta").await;

    // Re-register via the MCP tool — this is the explicit reactivation
    // path. The plan body is unchanged on disk, so notify won't fire
    // for the feedback file; the drift scan must replay it against the
    // freshly-started plan.
    let r2 = app
        .call(
            "register_plan_file",
            &app.repo,
            Some("m"),
            json!({"session_id": "beta", "path": &plan_path}),
        )
        .await
        .unwrap();
    let new_plan_id = r2["plan_id"].as_i64().unwrap();
    assert_ne!(
        new_plan_id, plan_id,
        "reactivation must start a new plan, not revive the old one"
    );
    let _ = rev_id;

    tokio::time::sleep(SETTLE).await;

    // The same feedback file should now produce a feedback row against
    // the NEW plan. Without the drift scan + sidecar reset this would
    // be 0 (the dispatch's content-hash no-op guard would short-circuit
    // because the on-disk file is unchanged).
    let new_fb: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM feedback WHERE plan_id = ? AND author_label = 'rev-a'",
    )
    .bind(new_plan_id)
    .fetch_one(&app.state.pool)
    .await
    .unwrap();
    assert_eq!(
        new_fb, 1,
        "reactivation drift scan must replay unchanged feedback files against the new plan"
    );
}

#[tokio::test]
async fn auto_register_basename_collision_across_repos_disambiguates() {
    // Two separate repos both contain `.trinity/plans/shared-name.md`.
    // The first auto-register uses the bare basename; the second must
    // get a disambiguated `shared-name-<6-hex>` session id.
    let app = TestApp::spawn().await;

    // Bootstrap repo A.
    let plan_a = drop_plan(&app.repo, "shared-name.md", "# a\n").await;
    app.call(
        "register_plan_file",
        &app.repo,
        Some("m"),
        json!({"session_id": "shared-name", "path": &plan_a}),
    )
    .await
    .unwrap();

    // Create a sibling repo with the same plan basename. Canonicalize
    // so notify event paths match what register_plan_file recorded —
    // on macOS `/var/folders/...` resolves to `/private/var/folders/...`
    // and the watcher records the canonical form.
    let other_repo_raw = app.tmp.path().join("other-repo");
    tokio::fs::create_dir_all(&other_repo_raw).await.unwrap();
    let other_repo = dunce::canonicalize(&other_repo_raw).unwrap();
    common::run_git(&other_repo, &["init", "-q"]);
    common::run_git(&other_repo, &["config", "user.email", "test@trinity"]);
    common::run_git(&other_repo, &["config", "user.name", "trinity-test"]);
    std::fs::write(other_repo.join(".gitkeep"), b"").unwrap();
    common::run_git(&other_repo, &["add", ".gitkeep"]);
    common::run_git(&other_repo, &["commit", "-q", "-m", "init"]);

    // The plan-dir watcher only attaches for known repos, so we must
    // teach Trinity about this repo first by registering a primer plan.
    // The primer creates a known repo_root; subsequent files in the
    // same plans dir then trigger auto-registration via the watcher.
    let primer = drop_plan(&other_repo, "primer.md", "# primer\n").await;
    app.call(
        "register_plan_file",
        &other_repo,
        Some("m"),
        json!({"session_id": "primer-other", "path": &primer}),
    )
    .await
    .unwrap();

    // Drop the colliding file. Live watcher should auto-register it
    // with a disambiguated id (basename + short repo hash).
    drop_plan(&other_repo, "shared-name.md", "# b\n").await;
    tokio::time::sleep(SETTLE).await;

    let disambiguated: Vec<String> = sqlx::query_scalar(
        "SELECT id FROM sessions WHERE id LIKE 'shared-name-%' AND repo_root = ?",
    )
    .bind(other_repo.to_string_lossy().into_owned())
    .fetch_all(&app.state.pool)
    .await
    .unwrap();
    assert_eq!(
        disambiguated.len(),
        1,
        "colliding basename should produce exactly one disambiguated session in the second repo: got {disambiguated:?}"
    );

    // And the bare basename must remain bound to repo A.
    let bare_repo: String =
        sqlx::query_scalar("SELECT repo_root FROM sessions WHERE id = 'shared-name'")
            .fetch_one(&app.state.pool)
            .await
            .unwrap();
    let app_repo_canonical = dunce::canonicalize(&app.repo).unwrap();
    assert_eq!(
        bare_repo,
        app_repo_canonical.to_string_lossy(),
        "bare basename should remain bound to the original repo"
    );
}

#[tokio::test]
async fn modifying_deleted_sessions_plan_file_does_not_reactivate() {
    // Filesystem events never reactivate. Delete a session whose plan
    // file remains in `.trinity/plans/`, then modify (or touch) that
    // file while the daemon is up. The session must stay archived.
    let app = TestApp::spawn().await;

    let plan_path = drop_plan(&app.repo, "gamma.md", "# gamma\n").await;
    app.call(
        "register_plan_file",
        &app.repo,
        Some("m"),
        json!({"session_id": "gamma", "path": &plan_path}),
    )
    .await
    .unwrap();
    delete_session(&app, "gamma").await;

    let archived_before: Option<i64> =
        sqlx::query_scalar("SELECT archived_at FROM sessions WHERE id = 'gamma'")
            .fetch_one(&app.state.pool)
            .await
            .unwrap();
    assert!(archived_before.is_some());

    // Mutate the plan file while it's still being watched. Any plan-dir
    // notify event for an existing archived session must be ignored.
    tokio::fs::write(&plan_path, "# gamma rewritten\n")
        .await
        .unwrap();
    tokio::time::sleep(SETTLE).await;

    let archived_after: Option<i64> =
        sqlx::query_scalar("SELECT archived_at FROM sessions WHERE id = 'gamma'")
            .fetch_one(&app.state.pool)
            .await
            .unwrap();
    let active_after: Option<i64> =
        sqlx::query_scalar("SELECT active_plan_id FROM sessions WHERE id = 'gamma'")
            .fetch_one(&app.state.pool)
            .await
            .unwrap();
    assert!(
        archived_after.is_some(),
        "editing the plan file of a deleted session must not reactivate it"
    );
    assert!(
        active_after.is_none(),
        "no new active plan should appear for an archived session whose file was edited"
    );
}

#[tokio::test]
async fn reactivation_attaches_companion_watchers_after_restart() {
    // After daemon restart, the pre-archive watcher subscriptions are
    // gone. Reactivating an archived session via a fresh drop must
    // re-attach the git-logs watcher and the feedback-dir watchers,
    // not just the per-file plan watcher. Without that, subsequent
    // commits or feedback drops are silently ignored.
    let app = TestApp::spawn().await;

    let plan_path = drop_plan(&app.repo, "epsilon.md", "# epsilon\n").await;
    app.call(
        "register_plan_file",
        &app.repo,
        Some("m"),
        json!({"session_id": "epsilon", "path": &plan_path}),
    )
    .await
    .unwrap();
    delete_session(&app, "epsilon").await;

    // Restart drops all watcher subscriptions for the archived session.
    let app = app.restart().await;

    // Fresh drop: remove + re-create the same basename. The watcher's
    // plan-dir seen-set, populated at attach time, now sees a Remove +
    // Create pair → emits PlanDirFileCreated for the re-creation.
    tokio::fs::remove_file(&plan_path).await.unwrap();
    tokio::time::sleep(SETTLE).await;
    drop_plan(&app.repo, "epsilon.md", "# epsilon v2\n").await;
    tokio::time::sleep(SETTLE).await;

    let new_plan_id: i64 =
        sqlx::query_scalar("SELECT active_plan_id FROM sessions WHERE id = 'epsilon'")
            .fetch_one(&app.state.pool)
            .await
            .unwrap();
    assert!(new_plan_id > 0, "session should be reactivated after fresh drop post-restart");

    // Now exercise the companion watchers: a commit must produce an
    // implementation_revisions row (proves git-logs watcher attached),
    // and a feedback file drop must ingest (proves feedback-dir watcher
    // attached).
    common::make_commit(&app.repo, "f.txt", "x\n");
    tokio::time::sleep(SETTLE).await;
    let impl_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM implementation_revisions WHERE plan_id = ?",
    )
    .bind(new_plan_id)
    .fetch_one(&app.state.pool)
    .await
    .unwrap();
    assert_eq!(
        impl_count, 1,
        "git-logs watcher must be attached after fresh-drop reactivation"
    );

    let canonical_repo = dunce::canonicalize(&app.repo).unwrap();
    let plan_fb_dir = canonical_repo
        .join(".trinity")
        .join("feedback")
        .join("epsilon")
        .join("plan");
    tokio::fs::create_dir_all(&plan_fb_dir).await.unwrap();
    tokio::fs::write(plan_fb_dir.join("rev.md"), "post-reactivation review\n")
        .await
        .unwrap();
    tokio::time::sleep(SETTLE).await;
    let fb_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM feedback WHERE session_id = 'epsilon' AND author_label = 'rev'",
    )
    .fetch_one(&app.state.pool)
    .await
    .unwrap();
    assert_eq!(
        fb_count, 1,
        "feedback-dir watcher must be attached after fresh-drop reactivation"
    );
}

#[tokio::test]
async fn new_plan_lifecycle_on_existing_session_emits_outerhtml_not_afterbegin() {
    // A `plan_revision_created` event for revision 1 of the SECOND
    // plan on a session must target the existing `<tr id="session-row-{id}">`
    // with `outerHTML`. The bug shape (count_for_plan == 1 alone) would
    // emit `afterbegin:#sessions-table-body` and double-insert the row.
    //
    // This test reads the home SSE stream directly and asserts the OOB
    // wrapper kind, which is the actual surface that had the bug.
    let app = TestApp::spawn().await;

    let plan_v1 = app.repo.join("plan-v1.md");
    std::fs::write(&plan_v1, "# v1\n").unwrap();
    app.call(
        "register_plan_file",
        &app.repo,
        Some("m"),
        json!({"session_id": "zeta", "path": &plan_v1}),
    )
    .await
    .unwrap();

    // Move into implementing so the body-change below triggers
    // ArchiveActivePlan + StartPlan (a new plan, revision 1).
    common::make_commit(&app.repo, "x.txt", "x\n");
    tokio::time::sleep(SETTLE).await;

    let cursor: i64 = sqlx::query_scalar("SELECT MAX(id) FROM events")
        .fetch_one(&app.state.pool)
        .await
        .unwrap();

    let stream_handle = tokio::spawn({
        let app_url = app.base.clone();
        let client = app.client.clone();
        async move {
            let url = format!("{app_url}/events?since={cursor}");
            let resp = client.get(&url).send().await.unwrap();
            use futures::StreamExt;
            let mut stream = resp.bytes_stream();
            let mut bytes = bytes::BytesMut::new();
            let _ = tokio::time::timeout(std::time::Duration::from_secs(8), async {
                while let Some(chunk) = stream.next().await {
                    if let Ok(c) = chunk {
                        bytes.extend_from_slice(&c);
                        if String::from_utf8_lossy(&bytes).contains("session-row-zeta") {
                            break;
                        }
                    }
                }
            })
            .await;
            String::from_utf8_lossy(&bytes).into_owned()
        }
    });

    // Settle so the stream attaches before we trigger the new lifecycle.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // New plan body via the MCP path. Reducer archives the old plan,
    // starts a new one in `planning`, revision_number = 1. The home
    // SSE fragment must dispatch this as REPLACE (outerHTML), not
    // INSERT (afterbegin).
    let plan_v2 = app.repo.join("plan-v2.md");
    std::fs::write(&plan_v2, "# v2 different\n").unwrap();
    app.call(
        "register_plan_file",
        &app.repo,
        Some("m"),
        json!({"session_id": "zeta", "path": &plan_v2}),
    )
    .await
    .unwrap();

    let body = stream_handle.await.unwrap();
    assert!(
        body.contains("session-row-zeta"),
        "SSE stream must deliver a fragment for the new lifecycle: got:\n{body}"
    );
    assert!(
        body.contains("hx-swap-oob=\"outerHTML:#session-row-zeta\""),
        "second plan's first revision must dispatch as outerHTML replace, not afterbegin insert; got:\n{body}"
    );
    assert!(
        !body.contains("hx-swap-oob=\"afterbegin:#sessions-table-body\""),
        "must NOT emit afterbegin for an existing session row; got:\n{body}"
    );
}

#[tokio::test]
async fn fresh_drop_after_delete_reactivates_via_filesystem() {
    // The auto-register contract: a true fresh drop (operator removes
    // the old plan file and creates a new one with the same basename)
    // SHOULD reactivate the archived session. The seen-set guard
    // distinguishes this from an in-place edit, which must stay
    // ignored.
    let app = TestApp::spawn().await;

    let plan_path = drop_plan(&app.repo, "delta.md", "# delta\n").await;
    app.call(
        "register_plan_file",
        &app.repo,
        Some("m"),
        json!({"session_id": "delta", "path": &plan_path}),
    )
    .await
    .unwrap();
    delete_session(&app, "delta").await;

    // Confirm archived.
    let archived: Option<i64> =
        sqlx::query_scalar("SELECT archived_at FROM sessions WHERE id = 'delta'")
            .fetch_one(&app.state.pool)
            .await
            .unwrap();
    assert!(archived.is_some());

    // Register a different active session in the same repo so we can
    // verify reactivation supersedes it (one-active-per-repo).
    let other_plan = drop_plan(&app.repo, "other-active.md", "# other\n").await;
    app.call(
        "register_plan_file",
        &app.repo,
        Some("m"),
        json!({"session_id": "other-active", "path": &other_plan}),
    )
    .await
    .unwrap();

    let other_active_before: Option<i64> =
        sqlx::query_scalar("SELECT active_plan_id FROM sessions WHERE id = 'other-active'")
            .fetch_one(&app.state.pool)
            .await
            .unwrap();
    assert!(other_active_before.is_some());

    // Fresh drop: remove the existing `delta.md` then recreate it.
    // The watcher's seen-set sees the path leave + come back, so the
    // re-creation reads as a fresh drop, not a modification.
    tokio::fs::remove_file(&plan_path).await.unwrap();
    tokio::time::sleep(SETTLE).await;
    drop_plan(&app.repo, "delta.md", "# delta v2\n").await;
    tokio::time::sleep(SETTLE).await;

    // Assert reactivation:
    let delta_state: (Option<i64>, Option<i64>) = sqlx::query_as(
        "SELECT active_plan_id, archived_at FROM sessions WHERE id = 'delta'",
    )
    .fetch_one(&app.state.pool)
    .await
    .unwrap();
    assert!(
        delta_state.0.is_some(),
        "fresh drop should reactivate the archived session: got active_plan_id = {:?}",
        delta_state.0
    );
    assert!(
        delta_state.1.is_none(),
        "reactivated session must clear archived_at"
    );

    // Assert one-active-per-repo: the previously-active session is
    // archived by the supersession step.
    let other_state: (Option<i64>, Option<i64>) = sqlx::query_as(
        "SELECT active_plan_id, archived_at FROM sessions WHERE id = 'other-active'",
    )
    .fetch_one(&app.state.pool)
    .await
    .unwrap();
    assert!(
        other_state.0.is_none(),
        "previously-active session must lose its active plan after reactivation"
    );
    assert!(
        other_state.1.is_some(),
        "previously-active session must be archived (one-active-per-repo)"
    );
}

#[tokio::test]
async fn recovery_disambiguates_cross_repo_collisions() {
    // Repo A already owns `shared-name`. While the daemon is down, a
    // `shared-name.md` lands in a second known repo's plans dir.
    // Restart recovery must disambiguate it as `shared-name-<repohash>`
    // for repo B — not skip it because the bare id already exists.
    let app = TestApp::spawn().await;

    // Repo A bootstrap.
    let plan_a = drop_plan(&app.repo, "shared-name.md", "# a\n").await;
    app.call(
        "register_plan_file",
        &app.repo,
        Some("m"),
        json!({"session_id": "shared-name", "path": &plan_a}),
    )
    .await
    .unwrap();

    // Repo B exists and is known (one prior session registered).
    let other_repo_raw = app.tmp.path().join("other-repo");
    tokio::fs::create_dir_all(&other_repo_raw).await.unwrap();
    let other_repo = dunce::canonicalize(&other_repo_raw).unwrap();
    common::run_git(&other_repo, &["init", "-q"]);
    common::run_git(&other_repo, &["config", "user.email", "test@trinity"]);
    common::run_git(&other_repo, &["config", "user.name", "trinity-test"]);
    std::fs::write(other_repo.join(".gitkeep"), b"").unwrap();
    common::run_git(&other_repo, &["add", ".gitkeep"]);
    common::run_git(&other_repo, &["commit", "-q", "-m", "init"]);
    let primer = drop_plan(&other_repo, "primer.md", "# primer\n").await;
    app.call(
        "register_plan_file",
        &other_repo,
        Some("m"),
        json!({"session_id": "primer-other", "path": &primer}),
    )
    .await
    .unwrap();

    // Drop the colliding file BEFORE restart so it's only the recovery
    // scan (not the live watcher) that has to handle it. The live
    // watcher could otherwise pick it up first and obscure the bug.
    drop_plan(&other_repo, "shared-name.md", "# b\n").await;

    let app = app.restart().await;
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let disambiguated: Vec<String> = sqlx::query_scalar(
        "SELECT id FROM sessions WHERE id LIKE 'shared-name-%' AND repo_root = ?",
    )
    .bind(other_repo.to_string_lossy().into_owned())
    .fetch_all(&app.state.pool)
    .await
    .unwrap();
    assert_eq!(
        disambiguated.len(),
        1,
        "recovery scan must disambiguate the cross-repo basename collision: got {disambiguated:?}"
    );
}

#[tokio::test]
async fn auto_register_reactivation_archives_other_active_session_in_repo() {
    // Repo has active session X. A `.md` file appears that reactivates
    // archived session Y in the same repo. After reactivation, X must
    // be archived to preserve the one-active-per-repo invariant.
    let app = TestApp::spawn().await;

    // Create + delete Y, leaving the plan file on disk.
    let plan_y = drop_plan(&app.repo, "yankee.md", "# y\n").await;
    app.call(
        "register_plan_file",
        &app.repo,
        Some("m"),
        json!({"session_id": "yankee", "path": &plan_y}),
    )
    .await
    .unwrap();
    delete_session(&app, "yankee").await;

    // Now register X — becomes the active session in the repo.
    let plan_x = drop_plan(&app.repo, "xray.md", "# x\n").await;
    app.call(
        "register_plan_file",
        &app.repo,
        Some("m"),
        json!({"session_id": "xray", "path": &plan_x}),
    )
    .await
    .unwrap();

    let x_active: Option<i64> =
        sqlx::query_scalar("SELECT active_plan_id FROM sessions WHERE id = 'xray'")
            .fetch_one(&app.state.pool)
            .await
            .unwrap();
    assert!(x_active.is_some(), "xray should be active before reactivation");

    // Re-register Y via the explicit MCP path — reactivation. The
    // dispatcher must then archive xray to preserve the invariant.
    app.call(
        "register_plan_file",
        &app.repo,
        Some("m"),
        json!({"session_id": "yankee", "path": &plan_y}),
    )
    .await
    .unwrap();

    let y_active: Option<i64> =
        sqlx::query_scalar("SELECT active_plan_id FROM sessions WHERE id = 'yankee'")
            .fetch_one(&app.state.pool)
            .await
            .unwrap();
    let x_active_after: Option<i64> =
        sqlx::query_scalar("SELECT active_plan_id FROM sessions WHERE id = 'xray'")
            .fetch_one(&app.state.pool)
            .await
            .unwrap();
    let x_archived: Option<i64> =
        sqlx::query_scalar("SELECT archived_at FROM sessions WHERE id = 'xray'")
            .fetch_one(&app.state.pool)
            .await
            .unwrap();

    assert!(y_active.is_some(), "reactivated session should have a new active plan");
    assert!(
        x_active_after.is_none(),
        "previous active session must lose its active plan after the supersession"
    );
    assert!(
        x_archived.is_some(),
        "previous active session must be archived (one-active-per-repo)"
    );
}
