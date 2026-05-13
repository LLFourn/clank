//! SSE live updates over the session events stream.
//! Drives events through the real watcher-coordinator paths (plan-file
//! edit, `git commit`, `.trinity/feedback/.../<author>.md` write) and
//! reads `text/event-stream` over a raw TCP byte read.

mod common;

use serde_json::json;

use common::{TestApp, make_commit};

use common::SETTLE;

async fn register(app: &TestApp) -> i64 {
    let plan_path = app.repo.join("plan.md");
    std::fs::write(&plan_path, "# v1\n").unwrap();
    let r = app
        .call(
            "register_plan_file",
            &app.repo,
            None,
            json!({"session_id": "s", "path": &plan_path, "label": "m"}),
        )
        .await
        .unwrap();
    r["revision_id"].as_i64().unwrap()
}

async fn collect_response_until(
    resp: reqwest::Response,
    needle: &str,
    dur: std::time::Duration,
) -> String {
    use futures::StreamExt;
    let mut stream = resp.bytes_stream();
    let mut bytes = bytes::BytesMut::new();
    let _ = tokio::time::timeout(dur, async {
        while let Some(chunk) = stream.next().await {
            if let Ok(c) = chunk {
                bytes.extend_from_slice(&c);
                if String::from_utf8_lossy(&bytes).contains(needle) {
                    break;
                }
            }
        }
    })
    .await;
    String::from_utf8_lossy(&bytes).into_owned()
}

#[tokio::test]
async fn home_sse_emits_row_and_timeline_for_auto_discovered_plan() {
    let app = TestApp::spawn().await;
    let _ = register(&app).await;
    let max_id: i64 = sqlx::query_scalar("SELECT MAX(id) FROM events")
        .fetch_one(&app.state.pool)
        .await
        .unwrap();

    let resp = app
        .client
        .get(format!("{}/events?since={max_id}", app.base))
        .send()
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let plans_dir = dunce::canonicalize(&app.repo)
        .unwrap()
        .join(".trinity")
        .join("plans");
    std::fs::create_dir_all(&plans_dir).unwrap();
    std::fs::write(plans_dir.join("new-session.md"), "# new session\n").unwrap();

    let body = collect_response_until(
        resp,
        "session-row-new-session",
        std::time::Duration::from_secs(8),
    )
    .await;
    assert!(
        body.contains("hx-swap-oob=\"afterbegin:#sessions-table-body\""),
        "home SSE must insert the new session row; got:\n{body}"
    );
    assert!(
        body.contains("id=\"session-row-new-session\""),
        "home SSE must include the new session row; got:\n{body}"
    );
    assert!(
        body.contains("hx-swap-oob=\"afterbegin:#home-timeline-feed\""),
        "home SSE must prepend the home timeline row that drives sound/animation; got:\n{body}"
    );
}

#[tokio::test]
async fn sse_emits_oob_fragment_for_new_plan_revision() {
    let app = TestApp::spawn().await;
    let _ = register(&app).await;
    let max_id: i64 = sqlx::query_scalar("SELECT MAX(id) FROM events")
        .fetch_one(&app.state.pool)
        .await
        .unwrap();

    let stream_handle = tokio::spawn({
        let app_url = app.base.clone();
        let client = app.client.clone();
        async move {
            let url = format!("{app_url}/sessions/s/events?since={max_id}");
            let resp = client.get(&url).send().await.unwrap();
            use futures::StreamExt;
            let mut stream = resp.bytes_stream();
            let mut bytes = bytes::BytesMut::new();
            let _ = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                while let Some(chunk) = stream.next().await {
                    if let Ok(c) = chunk {
                        bytes.extend_from_slice(&c);
                        if String::from_utf8_lossy(&bytes).contains("plan_revision") {
                            break;
                        }
                    }
                }
            })
            .await;
            String::from_utf8_lossy(&bytes).into_owned()
        }
    });

    // Settle slightly so the stream attaches, then edit.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let plan_path = app.repo.join("plan.md");
    std::fs::write(&plan_path, "# v2\n").unwrap();

    let body = stream_handle.await.unwrap();
    assert!(
        body.contains("entry plan-rev") && body.contains("event-"),
        "expected SSE fragment for plan_revision_created; got:\n{body}"
    );
    assert!(
        body.contains(r#"class="entry plan-rev live""#),
        "live SSE rows must carry the live class that triggers insertion animation; got:\n{body}"
    );
    assert!(
        body.contains("View plan revision #2"),
        "SSE plan_revision fragment must carry click-through action pill; got:\n{body}"
    );
    assert!(
        body.contains("hx-swap-oob=\"afterbegin:#timeline-feed\""),
        "SSE fragment must declare OOB swap target; got:\n{body}"
    );
    assert!(
        body.contains("<template hx-swap-oob=\"afterbegin:#timeline-feed\"><article"),
        "OOB swap must wrap the article so htmx does not strip the live row root; got:\n{body}"
    );
}

#[tokio::test]
async fn sse_emits_oob_fragment_for_new_impl_commit() {
    let app = TestApp::spawn().await;
    let _ = register(&app).await;
    let max_id: i64 = sqlx::query_scalar("SELECT MAX(id) FROM events")
        .fetch_one(&app.state.pool)
        .await
        .unwrap();
    let stream_handle = tokio::spawn({
        let app_url = app.base.clone();
        let client = app.client.clone();
        async move {
            let url = format!("{app_url}/sessions/s/events?since={max_id}");
            let resp = client.get(&url).send().await.unwrap();
            use futures::StreamExt;
            let mut stream = resp.bytes_stream();
            let mut bytes = bytes::BytesMut::new();
            let _ = tokio::time::timeout(std::time::Duration::from_secs(8), async {
                while let Some(chunk) = stream.next().await {
                    if let Ok(c) = chunk {
                        bytes.extend_from_slice(&c);
                        if String::from_utf8_lossy(&bytes).contains("impl-commit") {
                            break;
                        }
                    }
                }
            })
            .await;
            String::from_utf8_lossy(&bytes).into_owned()
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    make_commit(&app.repo, "f.txt", "x\n");
    let body = stream_handle.await.unwrap();
    assert!(body.contains("entry impl-commit"), "got:\n{body}");
    assert!(
        body.contains("Diff parent..commit"),
        "SSE impl_commit fragment must carry click-through action pill; got:\n{body}"
    );
}

#[tokio::test]
async fn sse_emits_oob_fragment_for_feedback_added() {
    let app = TestApp::spawn().await;
    let _ = register(&app).await;
    let max_id: i64 = sqlx::query_scalar("SELECT MAX(id) FROM events")
        .fetch_one(&app.state.pool)
        .await
        .unwrap();
    let stream_handle = tokio::spawn({
        let app_url = app.base.clone();
        let client = app.client.clone();
        async move {
            let url = format!("{app_url}/sessions/s/events?since={max_id}");
            let resp = client.get(&url).send().await.unwrap();
            use futures::StreamExt;
            let mut stream = resp.bytes_stream();
            let mut bytes = bytes::BytesMut::new();
            let _ = tokio::time::timeout(std::time::Duration::from_secs(8), async {
                while let Some(chunk) = stream.next().await {
                    if let Ok(c) = chunk {
                        bytes.extend_from_slice(&c);
                        if String::from_utf8_lossy(&bytes).contains("entry feedback") {
                            break;
                        }
                    }
                }
            })
            .await;
            String::from_utf8_lossy(&bytes).into_owned()
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let canonical_repo = dunce::canonicalize(&app.repo).unwrap();
    let plan_dir = canonical_repo
        .join(".trinity")
        .join("feedback")
        .join("s")
        .join("plan");
    std::fs::write(plan_dir.join("rev-a.md"), "ping\n").unwrap();
    let body = stream_handle.await.unwrap();
    assert!(body.contains("entry feedback"), "got:\n{body}");
    assert!(
        body.contains("Open in plan revision #") && body.contains("#feedback-"),
        "SSE feedback fragment must carry click-through action pill with #feedback- anchor; got:\n{body}"
    );
}

#[tokio::test]
async fn sse_does_not_emit_for_noop_put_feedback() {
    use trinity::domain::FeedbackTargetRef;
    let app = TestApp::spawn().await;
    let rev_id = register(&app).await;
    // First put_feedback to set the baseline.
    app.put_feedback_via_service(
        "s",
        "rev",
        FeedbackTargetRef::PlanRevision(rev_id),
        "same body",
    )
    .await
    .unwrap();
    tokio::time::sleep(SETTLE).await;
    let max_id: i64 = sqlx::query_scalar("SELECT MAX(id) FROM events")
        .fetch_one(&app.state.pool)
        .await
        .unwrap();

    // Stream attaches and waits for any event. No-op put_feedback must
    // NOT emit one.
    let stream_handle = tokio::spawn({
        let app_url = app.base.clone();
        let client = app.client.clone();
        async move {
            let url = format!("{app_url}/sessions/s/events?since={max_id}");
            let resp = client.get(&url).send().await.unwrap();
            use futures::StreamExt;
            let mut stream = resp.bytes_stream();
            let mut bytes = bytes::BytesMut::new();
            let _ = tokio::time::timeout(std::time::Duration::from_secs(3), async {
                while let Some(chunk) = stream.next().await {
                    if let Ok(c) = chunk {
                        bytes.extend_from_slice(&c);
                    }
                }
            })
            .await;
            String::from_utf8_lossy(&bytes).into_owned()
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    // Re-post with the SAME body — should be a service-level no-op + no
    // ping.
    app.put_feedback_via_service(
        "s",
        "rev",
        FeedbackTargetRef::PlanRevision(rev_id),
        "same body",
    )
    .await
    .unwrap();
    let body = stream_handle.await.unwrap();
    assert!(
        !body.contains("entry feedback"),
        "no SSE feedback event should fire on a no-op; got:\n{body}"
    );
}

#[tokio::test]
async fn sse_without_cursor_streams_live_events_from_now() {
    let app = TestApp::spawn().await;
    let _ = register(&app).await;
    let stream_handle = tokio::spawn({
        let app_url = app.base.clone();
        let client = app.client.clone();
        async move {
            let resp = client
                .get(format!("{app_url}/sessions/s/events"))
                .send()
                .await
                .unwrap();
            collect_response_until(resp, "entry plan-rev", std::time::Duration::from_secs(5)).await
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    std::fs::write(app.repo.join("plan.md"), "# v2\n").unwrap();
    let body = stream_handle.await.unwrap();
    assert!(
        body.contains("entry plan-rev") && body.contains("View plan revision #2"),
        "SSE without a cursor should stream future live events; got:\n{body}"
    );
}

#[tokio::test]
async fn sse_cursor_honours_last_event_id_on_reconnect() {
    let app = TestApp::spawn().await;
    let _ = register(&app).await;
    let cursor: i64 = sqlx::query_scalar("SELECT MAX(id) FROM events")
        .fetch_one(&app.state.pool)
        .await
        .unwrap();
    std::fs::write(app.repo.join("plan.md"), "# v2\n").unwrap();
    tokio::time::sleep(SETTLE).await;

    let resp = app
        .client
        .get(format!("{}/sessions/s/events", app.base))
        .header("last-event-id", cursor.to_string())
        .send()
        .await
        .unwrap();
    let body = collect_response_until(resp, "Diff to #1", std::time::Duration::from_secs(3)).await;
    assert!(
        body.contains("entry plan-rev") && body.contains("Diff to #1"),
        "reconnect should immediately flush rows after Last-Event-ID; got:\n{body}"
    );
}

#[tokio::test]
async fn sse_since_cursor_flushes_multiple_pending_rows() {
    let app = TestApp::spawn().await;
    let _ = register(&app).await;
    let cursor: i64 = sqlx::query_scalar("SELECT MAX(id) FROM events")
        .fetch_one(&app.state.pool)
        .await
        .unwrap();
    let plan_path = app.repo.join("plan.md");
    for n in 2..=4 {
        std::fs::write(&plan_path, format!("# v{n}\n")).unwrap();
        tokio::time::sleep(SETTLE).await;
    }

    let resp = app
        .client
        .get(format!("{}/sessions/s/events?since={cursor}", app.base))
        .send()
        .await
        .unwrap();
    let body = collect_response_until(resp, "Diff to #3", std::time::Duration::from_secs(3)).await;
    assert!(
        body.contains("Diff to #1") && body.contains("Diff to #2") && body.contains("Diff to #3"),
        "cursor catch-up should flush all pending rows; got:\n{body}"
    );
}

#[tokio::test]
async fn sse_events_for_unknown_session_returns_404() {
    let app = TestApp::spawn().await;
    let resp = app.get("/sessions/nope/events").await;
    assert_eq!(resp.status(), 404);
}
