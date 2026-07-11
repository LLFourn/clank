//! GitHub event wake sources for `clank wait` (extra-wait-events M3).
//!
//! Polls a repo's `/events` feed and maps activity to
//! [`WaitItem::GithubEvent`]s. The classification, pagination, and
//! cursor logic are PURE and fixture-tested; the transport is
//! INJECTED (`EventFetcher`) so the whole poll state machine is driven
//! deterministically in tests, and the production fetcher is an
//! async, cancellation-safe `gh` child (killed on task abort).
//!
//! ## Cursor model
//!
//! Event ids are identifiers, NOT an ordering (GitHub's Events API is
//! a bounded, DELAYED timeline — an unseen event can surface behind a
//! seen one). The cursor is a bounded SEEN-ID SET: the arm-time poll
//! pages the whole bounded feed into the set and emits nothing
//! (delta-from-now); each later poll emits events whose id is NOT in
//! the set. Pagination (ETag conditions ONLY page 1) continues through
//! mixed pages until a page holds ONLY seen ids (the boundary — NOT
//! merely one seen id, since a delayed unseen event can sit behind it)
//! or an empty page (clean exhaustion). Hitting the page cap without a
//! boundary is OVERRUN — emit what was fetched and warn of a possible
//! gap.

use std::collections::{BTreeSet, HashSet, VecDeque};
use std::future::Future;
use std::time::Duration;

use clank_core::agent_config::{GithubEventKind, GithubSource};
use clank_core::wait::WaitItem;

/// GitHub's `/events` feed caps at ~300 events (3 pages of 100).
const MAX_PAGES: u32 = 3;
/// Bound the seen-ID set so a long-lived poll can't grow unbounded.
const SEEN_CAP: usize = 1000;

/// One classified event, or `None` if it doesn't match a wanted
/// sub-kind, is filtered by actor, or is a non-creating action (an
/// edit/delete/dismiss is not a new comment/review). Pure.
pub(crate) fn classify_event(
    repo: &str,
    event: &serde_json::Value,
    wanted: &[GithubEventKind],
    own_login: Option<&str>,
    include_own: bool,
) -> Option<WaitItem> {
    let actor = event
        .get("actor")
        .and_then(|a| a.get("login"))
        .and_then(|l| l.as_str());
    if !include_own
        && let (Some(own), Some(actor)) = (own_login, actor)
        && own == actor
    {
        return None;
    }

    let ty = event.get("type").and_then(|t| t.as_str())?;
    let payload = event.get("payload").unwrap_or(&serde_json::Value::Null);
    let action = payload.get("action").and_then(|a| a.as_str());
    let pr = || {
        payload
            .get("pull_request")
            .unwrap_or(&serde_json::Value::Null)
    };

    // Action names follow the REST Events API (docs.github.com
    // github-event-types), which differs from webhook payloads. A
    // merge is `action == "closed"` with `pull_request.merged == true`
    // (the API's shape); `"merged"` is accepted too for
    // forward/legacy tolerance.
    let (kind, detail, obj): (GithubEventKind, Option<&str>, &serde_json::Value) = match ty {
        "PullRequestEvent" => {
            let merged = pr().get("merged").and_then(|m| m.as_bool()) == Some(true);
            match action {
                Some("opened") => (GithubEventKind::PrOpened, None, pr()),
                Some("merged") => (GithubEventKind::PrMerged, None, pr()),
                Some("closed") if merged => (GithubEventKind::PrMerged, None, pr()),
                _ => return None,
            }
        }
        "IssuesEvent" => {
            let issue = payload.get("issue").unwrap_or(&serde_json::Value::Null);
            match action {
                Some("opened") => (GithubEventKind::IssueOpened, None, issue),
                Some("closed") => (GithubEventKind::IssueClosed, None, issue),
                _ => return None,
            }
        }
        // Events API IssueCommentEvent actions: created/edited/deleted —
        // only a CREATED comment is a new comment.
        "IssueCommentEvent" if action == Some("created") => {
            let issue = payload.get("issue").unwrap_or(&serde_json::Value::Null);
            if issue.get("pull_request").is_some() {
                (
                    GithubEventKind::PrComment,
                    Some("issue_comment_on_pr"),
                    issue,
                )
            } else {
                (GithubEventKind::IssueComment, None, issue)
            }
        }
        // Events API PullRequestReviewEvent action is `created` (a new
        // submitted review, empty-body approvals included), NOT the
        // webhook `submitted`; accept both for tolerance.
        "PullRequestReviewEvent" if matches!(action, Some("created") | Some("submitted")) => {
            (GithubEventKind::PrComment, Some("review"), pr())
        }
        // Events API PullRequestReviewCommentEvent action: created.
        "PullRequestReviewCommentEvent" if action == Some("created") => {
            (GithubEventKind::PrComment, Some("review_comment"), pr())
        }
        _ => return None,
    };

    if !wanted.contains(&kind) {
        return None;
    }
    Some(WaitItem::GithubEvent {
        repo: repo.to_string(),
        event: kind_str(kind).to_string(),
        detail: detail.map(str::to_string),
        number: obj.get("number").and_then(|n| n.as_u64()),
        title: obj
            .get("title")
            .and_then(|t| t.as_str())
            .map(str::to_string),
        actor: actor.map(str::to_string),
        url: obj
            .get("html_url")
            .and_then(|u| u.as_str())
            .map(str::to_string),
    })
}

fn kind_str(kind: GithubEventKind) -> &'static str {
    match kind {
        GithubEventKind::PrOpened => "pr_opened",
        GithubEventKind::PrMerged => "pr_merged",
        GithubEventKind::PrComment => "pr_comment",
        GithubEventKind::IssueOpened => "issue_opened",
        GithubEventKind::IssueClosed => "issue_closed",
        GithubEventKind::IssueComment => "issue_comment",
    }
}

/// New items from one page (ids not in `seen`, matching + filtered),
/// plus the ids observed on the page. Pure.
fn map_page(
    src: &GithubSource,
    events: &[serde_json::Value],
    seen: &BTreeSet<String>,
    own_login: Option<&str>,
) -> (Vec<WaitItem>, Vec<String>) {
    let mut items = Vec::new();
    let mut ids = Vec::new();
    for event in events {
        let Some(id) = event.get("id").and_then(|i| i.as_str()) else {
            continue;
        };
        ids.push(id.to_string());
        if seen.contains(id) {
            continue;
        }
        if let Some(item) = classify_event(
            &src.repo,
            event,
            &src.events,
            own_login,
            src.include_own_actions,
        ) {
            items.push(item);
        }
    }
    (items, ids)
}

/// A bounded, insertion-ordered seen-ID set (the cursor). Evicts the
/// oldest ids past [`SEEN_CAP`].
pub(crate) struct SeenSet {
    set: HashSet<String>,
    order: VecDeque<String>,
    cap: usize,
}

impl SeenSet {
    fn new(cap: usize) -> Self {
        Self {
            set: HashSet::new(),
            order: VecDeque::new(),
            cap,
        }
    }
    fn as_btree(&self) -> BTreeSet<String> {
        self.set.iter().cloned().collect()
    }
    fn extend(&mut self, ids: impl IntoIterator<Item = String>) {
        for id in ids {
            if self.set.insert(id.clone()) {
                self.order.push_back(id);
                while self.order.len() > self.cap {
                    if let Some(old) = self.order.pop_front() {
                        self.set.remove(&old);
                    }
                }
            }
        }
    }
    #[cfg(test)]
    fn len(&self) -> usize {
        self.set.len()
    }
}

/// One `/events` response the fetcher yields.
pub(crate) enum GhResponse {
    /// `304 Not Modified` (only meaningful for the ETag-conditioned
    /// page 1). Carries the response's `X-Poll-Interval`: the server
    /// can raise its floor ON a 304 and the client must still honor
    /// it (codex e9b8b66).
    NotModified { poll_interval: Option<u64> },
    Ok {
        events: Vec<serde_json::Value>,
        etag: Option<String>,
        /// The server's `X-Poll-Interval` in seconds, if present.
        poll_interval: Option<u64>,
    },
}

/// The injected transport: fetch one page (`page` is 1-based; only
/// page 1 carries `etag` for a conditional request). Async so the
/// production impl is a cancellation-safe child.
pub(crate) trait EventFetcher {
    fn fetch(
        &self,
        page: u32,
        etag: Option<String>,
    ) -> impl Future<Output = anyhow::Result<GhResponse>> + Send;
}

/// One poll tick's outcome. Server metadata rides OUTSIDE the result
/// so page-1 headers survive a deeper page's failure — the server's
/// interval floor binds even on a degraded tick (codex e9b8b66).
pub(crate) struct PollOutcome {
    /// Page-1 `X-Poll-Interval` (also carried by a 304).
    pub(crate) poll_interval: Option<u64>,
    /// Page-1 ETag. The caller must apply it ONLY on a successful
    /// tick: conditioning the next tick with the ETag of a FAILED one
    /// would 304 straight past the unread deeper pages' events.
    pub(crate) etag: Option<String>,
    pub(crate) result: anyhow::Result<PollDelta>,
}

pub(crate) enum PollDelta {
    NotModified,
    Fetched {
        items: Vec<WaitItem>,
        new_ids: Vec<String>,
        overrun: bool,
    },
}

/// Run ONE poll tick against `seen` (immutable — every page's
/// boundary check is against the pre-tick cursor): condition page 1
/// with `etag`, then paginate until a fully-seen boundary or an empty
/// page, capped at [`MAX_PAGES`] (hitting the cap = overrun). Pure
/// over the injected fetcher, so the whole state machine is testable
/// without `gh`.
pub(crate) async fn poll_once<F: EventFetcher>(
    src: &GithubSource,
    seen: &BTreeSet<String>,
    etag: Option<&str>,
    own_login: Option<&str>,
    fetcher: &F,
) -> PollOutcome {
    let mut items = Vec::new();
    let mut new_ids = Vec::new();
    let mut resp_etag: Option<String> = None;
    let mut poll_interval: Option<u64> = None;
    let mut boundary = false;
    // Whether ANY seen id was encountered — distinct from the all-seen
    // stop condition. Reaching the cap is only true OVERRUN (accepted
    // loss) if we NEVER met a seen id: a mixed final page means the
    // cursor was reached, older events are all seen, nothing lost
    // (codex a989653).
    let mut saw_seen = false;

    for page in 1..=MAX_PAGES {
        let cond = if page == 1 {
            etag.map(str::to_string)
        } else {
            None
        };
        let response = match fetcher.fetch(page, cond).await {
            Ok(r) => r,
            // A failed page still surfaces the page-1 metadata already
            // captured — the server floor binds even on a degraded tick.
            Err(e) => {
                return PollOutcome {
                    poll_interval,
                    etag: resp_etag,
                    result: Err(e),
                };
            }
        };
        let events = match response {
            // Only page 1 is conditioned; a 304 THERE means nothing
            // new at all. A 304 on a later (unconditioned) page can't
            // happen against real GitHub — treat it as end-of-feed
            // rather than discarding pages already fetched this tick.
            GhResponse::NotModified { poll_interval: pi } if page == 1 => {
                return PollOutcome {
                    poll_interval: pi,
                    etag: None,
                    result: Ok(PollDelta::NotModified),
                };
            }
            GhResponse::NotModified { .. } => {
                boundary = true;
                break;
            }
            GhResponse::Ok {
                events,
                etag,
                poll_interval: pi,
            } => {
                if page == 1 {
                    resp_etag = etag;
                    poll_interval = pi;
                }
                events
            }
        };
        if events.is_empty() {
            boundary = true; // clean exhaustion — reached the end
            break;
        }
        // map_page captures EVERY unseen id on the page regardless of
        // position, so a delayed event surfacing BEHIND a seen one on
        // the same page is not skipped. The boundary is a page holding
        // ONLY seen ids (not merely ANY seen id — a later page can
        // still carry a delayed unseen event behind a seen one; codex
        // ef1861a): keep paginating through mixed pages until a fully-
        // seen page, an empty page, or the cap.
        let ids_seen = events.iter().filter(|e| {
            e.get("id")
                .and_then(|i| i.as_str())
                .is_some_and(|id| seen.contains(id))
        });
        let page_seen_count = ids_seen.count();
        if page_seen_count > 0 {
            saw_seen = true;
        }
        let all_seen = page_seen_count == events.len();
        let (page_items, page_ids) = map_page(src, &events, seen, own_login);
        items.extend(page_items);
        new_ids.extend(page_ids);
        if all_seen {
            boundary = true;
            break;
        }
    }
    PollOutcome {
        poll_interval,
        etag: resp_etag,
        result: Ok(PollDelta::Fetched {
            items,
            new_ids,
            // Reached the page cap having NEVER met a seen id → older
            // events may have scrolled off the bounded timeline. A
            // mixed page (some seen) means the cursor was reached —
            // not loss.
            overrun: !boundary && !saw_seen,
        }),
    }
}

/// The effective poll interval: the server's `X-Poll-Interval` is a
/// FLOOR the client must obey, so take the max of it and the
/// configured cadence (default 60s).
pub(crate) fn effective_interval(
    configured: Option<Duration>,
    server_floor: Option<u64>,
) -> Duration {
    let configured = configured.unwrap_or(Duration::from_secs(60));
    let server = server_floor
        .map(Duration::from_secs)
        .unwrap_or(Duration::ZERO);
    configured.max(server)
}

/// Parse `gh api --include` output (status line, headers, blank line,
/// JSON body) into a [`GhResponse`]. Pure.
pub(crate) fn parse_gh_include(stdout: &str) -> anyhow::Result<GhResponse> {
    let mut lines = stdout.lines();
    let status = lines.next().unwrap_or("");
    // Headers are parsed BEFORE classifying the status: a 304 can
    // still raise `X-Poll-Interval` and the client must observe it
    // (codex e9b8b66).
    let not_modified = status.contains(" 304");
    let mut etag = None;
    let mut poll_interval = None;
    for line in lines.by_ref() {
        // `lines()` strips `\n` but keeps `\r`, so a CRLF blank
        // separator arrives as "\r" — trim it or the JSON body would be
        // parsed as headers and dropped (codex ef1861a).
        if line.trim_end_matches('\r').is_empty() {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let (name, value) = (name.trim().to_ascii_lowercase(), value.trim());
        match name.as_str() {
            "etag" => etag = Some(value.to_string()),
            "x-poll-interval" => poll_interval = value.parse().ok(),
            _ => {}
        }
    }
    if not_modified {
        return Ok(GhResponse::NotModified { poll_interval });
    }
    let body: String = lines.collect::<Vec<_>>().join("\n");
    let events: Vec<serde_json::Value> = if body.trim().is_empty() {
        Vec::new()
    } else {
        serde_json::from_str(&body).map_err(|e| anyhow::anyhow!("gh /events body not JSON: {e}"))?
    };
    Ok(GhResponse::Ok {
        events,
        etag,
        poll_interval,
    })
}

/// Production fetcher: an async, cancellation-safe `gh api --include`
/// child (`kill_on_drop`, so aborting the source task kills the poll).
struct GhFetcher {
    repo: String,
}

impl EventFetcher for GhFetcher {
    async fn fetch(&self, page: u32, etag: Option<String>) -> anyhow::Result<GhResponse> {
        let mut cmd = tokio::process::Command::new("gh");
        cmd.arg("api").arg("--include").arg(format!(
            "repos/{}/events?per_page=100&page={page}",
            self.repo
        ));
        if let Some(tag) = etag {
            cmd.arg("-H").arg(format!("If-None-Match: {tag}"));
        }
        cmd.kill_on_drop(true);
        let out = cmd.output().await?;
        if !out.status.success() {
            anyhow::bail!(
                "gh /events failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        parse_gh_include(&String::from_utf8_lossy(&out.stdout))
    }
}

/// The authenticated `gh` login, for the own-actor filter — an async,
/// cancellation-safe child. `Err` when `gh` can't answer (the caller
/// warns and degrades).
async fn gh_login() -> anyhow::Result<String> {
    let out = tokio::process::Command::new("gh")
        .args(["api", "user", "--jq", ".login"])
        .kill_on_drop(true)
        .output()
        .await?;
    if !out.status.success() {
        anyhow::bail!(
            "gh api user failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let login = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if login.is_empty() {
        anyhow::bail!("gh returned an empty login");
    }
    Ok(login)
}

/// Resolves the authenticated login for the own-action filter —
/// injected so a login failure-then-success is testable.
pub(crate) trait LoginResolver {
    fn resolve(&self) -> impl Future<Output = anyhow::Result<String>> + Send;
}

/// Production resolver: `gh api user`.
struct GhLogin;
impl LoginResolver for GhLogin {
    async fn resolve(&self) -> anyhow::Result<String> {
        gh_login().await
    }
}

/// One github wake source: the production entry point. Drives
/// [`poll_loop`] with the real `gh` fetcher + login resolver.
pub(crate) async fn run_github_source(
    src: GithubSource,
    tx: tokio::sync::mpsc::UnboundedSender<WaitItem>,
) {
    let fetcher = GhFetcher {
        repo: src.repo.clone(),
    };
    poll_loop(&src, &GhLogin, &fetcher, &tx).await;
}

/// The transport-agnostic poll loop: the arm-time fetch establishes
/// the baseline (emits nothing), then each tick emits new events.
/// State — seen cursor, ETag, and the remembered server interval
/// floor — persists across 304s and errors, so a failed tick never
/// resets delta-from-now or drops below the server floor.
///
/// When the own-action filter is requested (`include_own_actions ==
/// false`) the loop DOES NOT poll until the login resolves: filtering
/// unavailable must NOT degrade to emitting the agent's own actions
/// (which would self-wake it in a loop). It retries login on the poll
/// cadence, warning each failure, and only begins polling once the
/// filter is established (codex a989653). Generic over the fetcher +
/// login resolver, so tested end-to-end without `gh`.
pub(crate) async fn poll_loop<F: EventFetcher, R: LoginResolver>(
    src: &GithubSource,
    login: &R,
    fetcher: &F,
    tx: &tokio::sync::mpsc::UnboundedSender<WaitItem>,
) {
    let configured = src
        .poll_interval
        .as_deref()
        .and_then(|s| crate::cli::wait::parse_duration_str(s).ok().flatten());
    let mut seen = SeenSet::new(SEEN_CAP);
    let mut etag: Option<String> = None;
    let mut floor: Option<u64> = None;
    let mut established = false; // has a baseline fetch succeeded?
    let mut own_login: Option<String> = None;

    loop {
        // Establish the own-action filter before ANY poll when it's
        // requested — never poll unfiltered.
        if !src.include_own_actions && own_login.is_none() {
            match login.resolve().await {
                Ok(l) => own_login = Some(l),
                Err(e) => {
                    eprintln!(
                        "wait: github {} — own-action filter needs the gh login ({e}); \
                         not polling this source until it resolves",
                        src.repo
                    );
                    tokio::time::sleep(effective_interval(configured, floor)).await;
                    continue;
                }
            }
        }
        let outcome = poll_once(
            src,
            &seen.as_btree(),
            etag.as_deref(),
            own_login.as_deref(),
            fetcher,
        )
        .await;
        // The server floor binds unconditionally — including on a 304
        // and on a tick whose deeper page FAILED (codex e9b8b66).
        floor = outcome.poll_interval.or(floor);
        match outcome.result {
            Ok(PollDelta::NotModified) => {}
            Ok(PollDelta::Fetched {
                items,
                new_ids,
                overrun,
            }) => {
                seen.extend(new_ids);
                etag = outcome.etag.or(etag);
                // Overrun is meaningless at the baseline tick (which
                // pages the whole bounded feed into the cursor by
                // design); only a DELTA poll that overran can have lost
                // events.
                if overrun && established {
                    eprintln!(
                        "wait: github {} — more than {MAX_PAGES} pages of new events since the \
                         last poll; older ones may have been missed",
                        src.repo
                    );
                }
                // The FIRST successful fetch is baseline-only
                // (delta-from-now); a failed arm-time fetch does NOT
                // count, so real history is never replayed as new.
                if established {
                    for item in items {
                        if tx.send(item).is_err() {
                            return; // wait gone
                        }
                    }
                }
                established = true;
            }
            Err(e) => {
                // outcome.etag is deliberately DROPPED here: the tick
                // didn't complete, so conditioning the next one with
                // its ETag would 304 straight past the unread deeper
                // pages' events. Retry the whole tick unconditioned.
                eprintln!("wait: github {} poll failed: {e}", src.repo);
            }
        }
        tokio::time::sleep(effective_interval(configured, floor)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn src(events: &[GithubEventKind], include_own: bool) -> GithubSource {
        GithubSource {
            repo: "o/r".into(),
            events: events.to_vec(),
            poll_interval: None,
            include_own_actions: include_own,
        }
    }

    /// A recorded-shape `/events` element.
    fn ev(id: &str, ty: &str, payload: serde_json::Value, actor: &str) -> serde_json::Value {
        serde_json::json!({ "id": id, "type": ty, "actor": {"login": actor}, "payload": payload })
    }

    #[test]
    fn classifies_all_six_sub_kinds_with_action_gating() {
        use GithubEventKind::*;
        let all = &[
            PrOpened,
            PrMerged,
            PrComment,
            IssueOpened,
            IssueClosed,
            IssueComment,
        ][..];
        let cases: Vec<(serde_json::Value, &str, Option<&str>)> = vec![
            (
                ev(
                    "1",
                    "PullRequestEvent",
                    serde_json::json!({"action":"opened","pull_request":{"number":1,"title":"a","html_url":"u1"}}),
                    "x",
                ),
                "pr_opened",
                None,
            ),
            (
                ev(
                    "2",
                    "PullRequestEvent",
                    serde_json::json!({"action":"closed","pull_request":{"number":2,"merged":true}}),
                    "x",
                ),
                "pr_merged",
                None,
            ),
            (
                ev(
                    "3",
                    "IssueCommentEvent",
                    serde_json::json!({"action":"created","issue":{"number":3,"pull_request":{}}}),
                    "x",
                ),
                "pr_comment",
                Some("issue_comment_on_pr"),
            ),
            (
                ev(
                    "4",
                    "PullRequestReviewEvent",
                    serde_json::json!({"action":"created","pull_request":{"number":4}}),
                    "x",
                ),
                "pr_comment",
                Some("review"),
            ),
            (
                ev(
                    "5",
                    "PullRequestReviewCommentEvent",
                    serde_json::json!({"action":"created","pull_request":{"number":5}}),
                    "x",
                ),
                "pr_comment",
                Some("review_comment"),
            ),
            (
                ev(
                    "6",
                    "IssuesEvent",
                    serde_json::json!({"action":"opened","issue":{"number":6}}),
                    "x",
                ),
                "issue_opened",
                None,
            ),
            (
                ev(
                    "7",
                    "IssuesEvent",
                    serde_json::json!({"action":"closed","issue":{"number":7}}),
                    "x",
                ),
                "issue_closed",
                None,
            ),
            (
                ev(
                    "8",
                    "IssueCommentEvent",
                    serde_json::json!({"action":"created","issue":{"number":8}}),
                    "x",
                ),
                "issue_comment",
                None,
            ),
        ];
        for (raw, want, detail) in cases {
            let item = classify_event("o/r", &raw, all, None, false)
                .unwrap_or_else(|| panic!("must classify {want}"));
            let WaitItem::GithubEvent {
                event, detail: d, ..
            } = item
            else {
                panic!("expected GithubEvent");
            };
            assert_eq!(event, want);
            assert_eq!(d.as_deref(), detail);
        }
    }

    #[test]
    fn pr_merged_accepts_both_the_api_and_legacy_shapes() {
        // Events API: action=closed + merged=true. Legacy/forward:
        // action=merged. Both map to pr_merged.
        for payload in [
            serde_json::json!({"action":"closed","pull_request":{"number":1,"merged":true}}),
            serde_json::json!({"action":"merged","pull_request":{"number":1}}),
        ] {
            let raw = ev("1", "PullRequestEvent", payload, "x");
            let item = classify_event("o/r", &raw, &[GithubEventKind::PrMerged], None, false);
            assert!(
                matches!(item, Some(WaitItem::GithubEvent { .. })),
                "must be pr_merged"
            );
        }
    }

    #[test]
    fn non_creating_actions_are_not_new_comments_or_reviews() {
        // Edited/deleted/dismissed activity must NOT be misreported.
        let wanted = &[GithubEventKind::PrComment, GithubEventKind::IssueComment];
        for (ty, action) in [
            ("IssueCommentEvent", "edited"),
            ("IssueCommentEvent", "deleted"),
            ("PullRequestReviewEvent", "dismissed"),
            ("PullRequestReviewEvent", "edited"),
            ("PullRequestReviewCommentEvent", "edited"),
            ("PullRequestReviewCommentEvent", "deleted"),
        ] {
            let raw = ev(
                "1",
                ty,
                serde_json::json!({"action":action,"issue":{"pull_request":{}},"pull_request":{}}),
                "x",
            );
            assert!(
                classify_event("o/r", &raw, wanted, None, false).is_none(),
                "{ty}/{action} must be filtered"
            );
        }
    }

    #[test]
    fn closed_unmerged_pr_and_own_actor_are_filtered() {
        let raw = ev(
            "9",
            "PullRequestEvent",
            serde_json::json!({"action":"closed","pull_request":{"number":9,"merged":false}}),
            "x",
        );
        assert!(classify_event("o/r", &raw, &[GithubEventKind::PrMerged], None, false).is_none());
        let raw = ev(
            "1",
            "PullRequestEvent",
            serde_json::json!({"action":"opened","pull_request":{"number":1}}),
            "me",
        );
        assert!(
            classify_event("o/r", &raw, &[GithubEventKind::PrOpened], Some("me"), false).is_none()
        );
        assert!(
            classify_event("o/r", &raw, &[GithubEventKind::PrOpened], Some("me"), true).is_some()
        );
    }

    #[test]
    fn seen_set_evicts_oldest_past_the_cap() {
        let mut s = SeenSet::new(2);
        s.extend(["a".into(), "b".into(), "c".into()]);
        assert_eq!(s.len(), 2, "bounded to cap");
        let bt = s.as_btree();
        assert!(bt.contains("b") && bt.contains("c"), "oldest 'a' evicted");
        assert!(!bt.contains("a"));
    }

    #[test]
    fn effective_interval_takes_the_server_floor() {
        assert_eq!(effective_interval(None, None), Duration::from_secs(60));
        assert_eq!(
            effective_interval(Some(Duration::from_secs(10)), Some(90)),
            Duration::from_secs(90)
        );
        assert_eq!(
            effective_interval(Some(Duration::from_secs(120)), Some(60)),
            Duration::from_secs(120)
        );
    }

    #[test]
    fn parse_gh_include_reads_304_etag_and_poll_interval() {
        // A 304 can still RAISE the server floor — headers are parsed
        // before the status is classified (codex e9b8b66).
        let nm = "HTTP/2.0 304 Not Modified\r\nETag: \"abc\"\r\nX-Poll-Interval: 120\r\n\r\n";
        match parse_gh_include(nm).unwrap() {
            GhResponse::NotModified { poll_interval } => {
                assert_eq!(poll_interval, Some(120), "the 304's floor is observed");
            }
            _ => panic!("expected NotModified"),
        }
        let ok = "HTTP/2.0 200 OK\nETag: \"xyz\"\nX-Poll-Interval: 45\n\n[{\"id\":\"1\",\"type\":\"IssuesEvent\",\"payload\":{}}]";
        match parse_gh_include(ok).unwrap() {
            GhResponse::Ok {
                events,
                etag,
                poll_interval,
            } => {
                assert_eq!(events.len(), 1);
                assert_eq!(etag.as_deref(), Some("\"xyz\""));
                assert_eq!(poll_interval, Some(45));
            }
            _ => panic!("expected Ok"),
        }
    }

    // ── injected-runner poll state machine ─────────────────────

    type Step = Box<dyn Fn(u32, Option<String>) -> anyhow::Result<GhResponse> + Send>;

    /// A scripted fetcher: a queue of per-`fetch(page, etag)` responses
    /// (each a closure so a test can assert the conditional ETag) and a
    /// log of the (page, etag) calls. When the script is exhausted it
    /// returns `304` forever, so a real `poll_loop` can keep ticking.
    struct ScriptFetcher {
        script: Mutex<VecDeque<Step>>,
        calls: Mutex<Vec<(u32, Option<String>)>>,
        /// Paused-clock instants of each fetch — lets a test assert the
        /// interval the loop actually slept (tokio time is
        /// deterministic under start_paused).
        instants: Mutex<Vec<tokio::time::Instant>>,
    }
    impl ScriptFetcher {
        fn new(steps: Vec<Step>) -> Self {
            Self {
                script: Mutex::new(steps.into()),
                calls: Mutex::new(Vec::new()),
                instants: Mutex::new(Vec::new()),
            }
        }
    }
    impl EventFetcher for ScriptFetcher {
        async fn fetch(&self, page: u32, etag: Option<String>) -> anyhow::Result<GhResponse> {
            self.calls.lock().unwrap().push((page, etag.clone()));
            self.instants
                .lock()
                .unwrap()
                .push(tokio::time::Instant::now());
            match self.script.lock().unwrap().pop_front() {
                Some(step) => step(page, etag),
                None => Ok(GhResponse::NotModified {
                    poll_interval: None,
                }),
            }
        }
    }

    /// A scripted login resolver: a queue of results (Err then Ok) so
    /// a failure-then-success is testable.
    struct ScriptLogin(Mutex<VecDeque<anyhow::Result<String>>>);
    impl LoginResolver for ScriptLogin {
        async fn resolve(&self) -> anyhow::Result<String> {
            self.0
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Ok("someone-else".to_string()))
        }
    }
    /// A login resolver that always succeeds with `login`.
    struct FixedLogin(&'static str);
    impl LoginResolver for FixedLogin {
        async fn resolve(&self) -> anyhow::Result<String> {
            Ok(self.0.to_string())
        }
    }

    fn ok(events: Vec<serde_json::Value>, etag: Option<&str>, pi: Option<u64>) -> GhResponse {
        GhResponse::Ok {
            events,
            etag: etag.map(str::to_string),
            poll_interval: pi,
        }
    }
    fn issue(id: &str, n: u64) -> serde_json::Value {
        ev(
            id,
            "IssuesEvent",
            serde_json::json!({"action":"opened","issue":{"number":n}}),
            "x",
        )
    }
    fn seen_of(ids: &[&str]) -> BTreeSet<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    #[tokio::test]
    async fn not_modified_yields_no_items_and_does_not_map() {
        let s = src(&[GithubEventKind::IssueOpened], false);
        let f = ScriptFetcher::new(vec![Box::new(|_p, etag| {
            assert_eq!(etag.as_deref(), Some("\"tag\""), "page 1 is conditioned");
            Ok(GhResponse::NotModified {
                poll_interval: None,
            })
        })]);
        match poll_once(&s, &seen_of(&["1"]), Some("\"tag\""), None, &f)
            .await
            .result
            .unwrap()
        {
            PollDelta::NotModified => {}
            _ => panic!("304 → NotModified"),
        }
    }

    #[tokio::test]
    async fn a_fully_seen_page_is_the_boundary() {
        // Page 1 holds ONLY seen ids → boundary, no page 2, no items.
        let s = src(&[GithubEventKind::IssueOpened], false);
        let f = ScriptFetcher::new(vec![Box::new(|_p, _e| {
            Ok(ok(
                vec![issue("2", 2), issue("1", 1)],
                Some("\"e\""),
                Some(50),
            ))
        })]);
        let outcome = poll_once(&s, &seen_of(&["1", "2"]), None, None, &f).await;
        assert_eq!(outcome.etag.as_deref(), Some("\"e\""));
        assert_eq!(outcome.poll_interval, Some(50));
        let PollDelta::Fetched { items, overrun, .. } = outcome.result.unwrap() else {
            panic!("fetched");
        };
        assert!(items.is_empty(), "nothing new");
        assert!(!overrun);
        assert_eq!(
            f.calls.lock().unwrap().len(),
            1,
            "stopped at the fully-seen page"
        );
    }

    #[tokio::test]
    async fn mixed_pages_paginate_until_a_fully_seen_page() {
        let s = src(&[GithubEventKind::IssueOpened], false);
        let f = ScriptFetcher::new(vec![
            Box::new(|_p, _e| Ok(ok(vec![issue("5", 5), issue("4", 4)], Some("\"e\""), None))),
            Box::new(|p, etag| {
                assert_eq!(p, 2);
                assert!(etag.is_none(), "only page 1 carries the ETag");
                Ok(ok(vec![issue("1", 1)], None, None))
            }),
        ]);
        let PollDelta::Fetched { items, overrun, .. } =
            poll_once(&s, &seen_of(&["1"]), None, None, &f)
                .await
                .result
                .unwrap()
        else {
            panic!("fetched");
        };
        assert_eq!(items.len(), 2, "5,4 from page 1");
        assert!(!overrun, "page 2 was fully seen");
    }

    #[tokio::test]
    async fn a_delayed_unseen_event_behind_the_boundary_is_not_skipped() {
        // codex ef1861a: GitHub's delayed timeline can surface a NEW
        // event on a LATER page, behind a page-1 seen id. An any-seen
        // shortcut would stop at page 1 and miss it; the all-seen
        // boundary keeps paginating and captures it.
        let s = src(&[GithubEventKind::IssueOpened], false);
        let f = ScriptFetcher::new(vec![
            // Page 1: a new event then a seen id (any-seen would stop).
            Box::new(|_p, _e| {
                Ok(ok(
                    vec![issue("7", 7), issue("old", 1)],
                    Some("\"e\""),
                    None,
                ))
            }),
            // Page 2: a DELAYED new event behind the boundary.
            Box::new(|_p, _e| Ok(ok(vec![issue("delayed", 6), issue("older", 2)], None, None))),
            // Page 3: fully seen → boundary.
            Box::new(|_p, _e| Ok(ok(vec![issue("x", 3)], None, None))),
        ]);
        let seen = seen_of(&["old", "older", "x"]);
        let PollDelta::Fetched { items, overrun, .. } =
            poll_once(&s, &seen, None, None, &f).await.result.unwrap()
        else {
            panic!("fetched");
        };
        let numbers: Vec<u64> = items
            .iter()
            .filter_map(|i| match i {
                WaitItem::GithubEvent { number, .. } => *number,
                _ => None,
            })
            .collect();
        assert!(
            numbers.contains(&7) && numbers.contains(&6),
            "the delayed id 6 is captured: {numbers:?}"
        );
        assert!(!overrun);
    }

    #[tokio::test]
    async fn hitting_the_page_cap_without_a_boundary_is_overrun() {
        let s = src(&[GithubEventKind::IssueOpened], false);
        let f = ScriptFetcher::new(
            (0..MAX_PAGES)
                .map(|i| {
                    let id = format!("{}", 100 + i);
                    Box::new(move |_p: u32, _e: Option<String>| {
                        Ok(ok(vec![issue(&id, 100 + i as u64)], None, None))
                    }) as Step
                })
                .collect(),
        );
        // Non-empty seen (none present on the pages) so pagination runs
        // to the cap without a fully-seen page.
        let PollDelta::Fetched { items, overrun, .. } =
            poll_once(&s, &seen_of(&["seed"]), None, None, &f)
                .await
                .result
                .unwrap()
        else {
            panic!("fetched");
        };
        assert_eq!(items.len(), MAX_PAGES as usize);
        assert!(overrun, "cap reached without a fully-seen page");
    }

    #[tokio::test]
    async fn empty_page_is_clean_exhaustion_not_overrun() {
        let s = src(&[GithubEventKind::IssueOpened], false);
        let f = ScriptFetcher::new(vec![
            Box::new(|_p, _e| Ok(ok(vec![issue("9", 9)], None, None))),
            Box::new(|_p, _e| Ok(ok(vec![], None, None))), // end of feed
        ]);
        let PollDelta::Fetched { items, overrun, .. } =
            poll_once(&s, &seen_of(&["seed"]), None, None, &f)
                .await
                .result
                .unwrap()
        else {
            panic!("fetched");
        };
        assert_eq!(items.len(), 1);
        assert!(!overrun, "empty page = reached the end, not loss");
    }

    #[tokio::test(start_paused = true)]
    async fn poll_loop_baselines_then_emits_new_events() {
        // The REAL poll_loop, paused-time so its inter-tick sleeps
        // resolve instantly. Tick 1 (baseline): a new event + a seen id
        // (fully-seen page 2 ends baseline) — emits NOTHING. Tick 2: a
        // genuinely new event → emitted. Then 304 forever; dropping the
        // receiver ends the loop.
        let f = ScriptFetcher::new(vec![
            // Tick 1 baseline: page 1 has id 1 (new at baseline) + a
            // marker seen id to end pagination.
            Box::new(|_p, _e| Ok(ok(vec![issue("1", 1)], Some("\"e1\""), None))),
            Box::new(|_p, _e| Ok(ok(vec![], None, None))), // baseline page 2 empty → done
            // Tick 2: id 2 new, then id 1 (now seen) ends pagination.
            Box::new(|_p, _e| Ok(ok(vec![issue("2", 2), issue("1", 1)], Some("\"e2\""), None))),
        ]);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let handle = tokio::spawn(async move {
            poll_loop(
                &src(&[GithubEventKind::IssueOpened], false),
                &FixedLogin("me"),
                &f,
                &tx,
            )
            .await;
        });
        // The first emitted item is id 2 (baseline id 1 was suppressed).
        let item = tokio::time::timeout(Duration::from_secs(300), rx.recv())
            .await
            .expect("an event is emitted within a few ticks")
            .expect("channel open");
        match item {
            WaitItem::GithubEvent { number, .. } => assert_eq!(number, Some(2)),
            other => panic!("expected the new event, got {other:?}"),
        }
        // Finish deterministically: abort the source task and join it
        // (codex a989653 — don't leave the JoinHandle to time out).
        handle.abort();
        let _ = handle.await;
    }

    #[test]
    fn maps_recorded_events_api_fixtures_for_all_kinds() {
        // A representative RECORDED Events API payload (all six
        // sub-kinds + the three pr_comment classes + an unrelated
        // WatchEvent) — codex a989653: map real shapes, not synthetic
        // minimal JSON.
        use GithubEventKind::*;
        let raw: Vec<serde_json::Value> =
            serde_json::from_str(include_str!("github_events_fixture.json")).unwrap();
        let s = GithubSource {
            repo: "o/r".into(),
            events: vec![
                PrOpened,
                PrMerged,
                PrComment,
                IssueOpened,
                IssueClosed,
                IssueComment,
            ],
            poll_interval: None,
            include_own_actions: true, // don't filter — assert full mapping
        };
        let (items, _ids) = map_page(&s, &raw, &BTreeSet::new(), None);
        let got: Vec<(&str, Option<&str>, Option<u64>)> = items
            .iter()
            .map(|i| match i {
                WaitItem::GithubEvent {
                    event,
                    detail,
                    number,
                    ..
                } => (event.as_str(), detail.as_deref(), *number),
                _ => panic!("github items only"),
            })
            .collect();
        assert_eq!(
            got,
            vec![
                ("pr_opened", None, Some(12)),
                ("pr_merged", None, Some(11)),
                ("pr_comment", Some("issue_comment_on_pr"), Some(12)),
                ("pr_comment", Some("review"), Some(12)),
                ("pr_comment", Some("review_comment"), Some(12)),
                ("issue_opened", None, Some(30)),
                ("issue_closed", None, Some(29)),
                ("issue_comment", None, Some(30)),
            ],
            "the WatchEvent is dropped; every other kind maps with its detail"
        );
        // The pr_opened item carries the recorded title + url.
        let WaitItem::GithubEvent { title, url, .. } = &items[0] else {
            unreachable!()
        };
        assert_eq!(title.as_deref(), Some("Add the widget"));
        assert_eq!(url.as_deref(), Some("https://github.com/o/r/pull/12"));
    }

    #[tokio::test]
    async fn a_mixed_final_page_at_the_cap_is_not_overrun() {
        // codex a989653: reaching the cap on a MIXED final page (some
        // seen ids) means the cursor WAS met — not accepted loss.
        let s = src(&[GithubEventKind::IssueOpened], false);
        let f = ScriptFetcher::new(vec![
            Box::new(|_p, _e| Ok(ok(vec![issue("9", 9)], None, None))),
            Box::new(|_p, _e| Ok(ok(vec![issue("8", 8)], None, None))),
            // Final page (cap) is MIXED: a new id AND a seen id.
            Box::new(|_p, _e| Ok(ok(vec![issue("7", 7), issue("seed", 1)], None, None))),
        ]);
        let PollDelta::Fetched { overrun, .. } = poll_once(&s, &seen_of(&["seed"]), None, None, &f)
            .await
            .result
            .unwrap()
        else {
            panic!("fetched");
        };
        assert!(!overrun, "a seen id on the final page means no loss");
    }

    #[tokio::test(start_paused = true)]
    async fn does_not_poll_until_the_own_action_filter_is_established() {
        // codex a989653: with include_own_actions=false, a login
        // failure must NOT let the source poll unfiltered. Login fails
        // once (no poll, no emit), then succeeds; only then does the
        // baseline+delta run, and the agent's OWN events stay
        // suppressed.
        let login = ScriptLogin(Mutex::new(
            vec![Err(anyhow::anyhow!("gh down")), Ok("me".to_string())].into(),
        ));
        // Own event (actor "me") + a foreign one; the foreign should be
        // the only thing that ever emits.
        let own = ev(
            "1",
            "IssuesEvent",
            serde_json::json!({"action":"opened","issue":{"number":1}}),
            "me",
        );
        let foreign_baseline = ev(
            "2",
            "IssuesEvent",
            serde_json::json!({"action":"opened","issue":{"number":2}}),
            "them",
        );
        let foreign_new = ev(
            "3",
            "IssuesEvent",
            serde_json::json!({"action":"opened","issue":{"number":3}}),
            "them",
        );
        let f = ScriptFetcher::new(vec![
            // First poll (after login succeeds) is the baseline: own +
            // foreign, then empty page to end pagination.
            Box::new(move |_p, _e| {
                Ok(ok(
                    vec![own.clone(), foreign_baseline.clone()],
                    Some("\"e\""),
                    None,
                ))
            }),
            Box::new(|_p, _e| Ok(ok(vec![], None, None))),
            // Delta: a new foreign event + a seen id to stop.
            Box::new(move |_p, _e| {
                Ok(ok(
                    vec![
                        foreign_new.clone(),
                        ev(
                            "2",
                            "IssuesEvent",
                            serde_json::json!({"action":"opened"}),
                            "them",
                        ),
                    ],
                    None,
                    None,
                ))
            }),
        ]);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let handle = tokio::spawn(async move {
            poll_loop(
                &src(&[GithubEventKind::IssueOpened], false),
                &login,
                &f,
                &tx,
            )
            .await;
        });
        let item = tokio::time::timeout(Duration::from_secs(300), rx.recv())
            .await
            .expect("a foreign event emits once the filter is established")
            .expect("channel open");
        match item {
            // ONLY the foreign new event (id 3) — never the own id 1.
            WaitItem::GithubEvent { number, .. } => assert_eq!(number, Some(3)),
            other => panic!("expected the foreign event, got {other:?}"),
        }
        handle.abort();
        let _ = handle.await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_page_two_failure_still_applies_the_page_one_floor() {
        // codex e9b8b66: page 1 raises the floor to 90s, page 2 FAILS.
        // The degraded tick must still bind the server floor — the next
        // fetch happens no sooner than 90s later (not the 60s default).
        // The failed tick's ETag must NOT condition the retry (it would
        // 304 past the unread pages).
        let f = std::sync::Arc::new(ScriptFetcher::new(vec![
            Box::new(|_p, _e| Ok(ok(vec![issue("1", 1)], Some("\"partial\""), Some(90)))),
            Box::new(|_p, _e| Err(anyhow::anyhow!("gh transport died"))),
            // The retry tick (page 1 again).
            Box::new(|_p, etag| {
                assert!(
                    etag.is_none(),
                    "a failed tick's ETag must not condition the retry"
                );
                Ok(ok(vec![], None, None))
            }),
        ]));
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let f2 = f.clone();
        let handle = tokio::spawn(async move {
            poll_loop(
                &src(&[GithubEventKind::IssueOpened], false),
                &FixedLogin("me"),
                &*f2,
                &tx,
            )
            .await;
        });
        // Wait (paused time auto-advances) until the retry fetch lands.
        while f.calls.lock().unwrap().len() < 3 {
            tokio::task::yield_now().await;
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        handle.abort();
        let _ = handle.await;
        let instants = f.instants.lock().unwrap();
        let gap = instants[2] - instants[1];
        assert!(
            gap >= Duration::from_secs(90),
            "the raised floor binds on the degraded tick: retry after {gap:?}, want ≥90s"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_304_floor_update_is_applied_to_the_next_sleep() {
        // codex e9b8b66: a 304 can RAISE X-Poll-Interval; the loop must
        // observe it — the tick after the 304 waits ≥120s.
        let f = std::sync::Arc::new(ScriptFetcher::new(vec![
            // Baseline: empty feed, no floor.
            Box::new(|_p, _e| Ok(ok(vec![], Some("\"e\""), None))),
            // 304 carrying a raised floor.
            Box::new(|_p, _e| {
                Ok(GhResponse::NotModified {
                    poll_interval: Some(120),
                })
            }),
            Box::new(|_p, _e| {
                Ok(GhResponse::NotModified {
                    poll_interval: None,
                })
            }),
        ]));
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let f2 = f.clone();
        let handle = tokio::spawn(async move {
            poll_loop(
                &src(&[GithubEventKind::IssueOpened], false),
                &FixedLogin("me"),
                &*f2,
                &tx,
            )
            .await;
        });
        while f.calls.lock().unwrap().len() < 3 {
            tokio::task::yield_now().await;
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        handle.abort();
        let _ = handle.await;
        let instants = f.instants.lock().unwrap();
        let gap = instants[2] - instants[1];
        assert!(
            gap >= Duration::from_secs(120),
            "the 304's raised floor binds: next tick after {gap:?}, want ≥120s"
        );
    }

    #[tokio::test]
    async fn poll_loop_cancels_a_pending_fetch_on_abort() {
        // Supervision (extra-wait-events): aborting the source task
        // must cancel a fetch in flight — the join returns promptly
        // rather than hanging on the pending future.
        struct PendingFetcher;
        impl EventFetcher for PendingFetcher {
            async fn fetch(&self, _p: u32, _e: Option<String>) -> anyhow::Result<GhResponse> {
                std::future::pending::<()>().await; // never resolves
                unreachable!()
            }
        }
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let handle = tokio::spawn(async move {
            poll_loop(
                &src(&[GithubEventKind::IssueOpened], false),
                &FixedLogin("me"),
                &PendingFetcher,
                &tx,
            )
            .await;
        });
        tokio::task::yield_now().await;
        handle.abort();
        let joined = tokio::time::timeout(Duration::from_secs(5), handle).await;
        assert!(
            joined.is_ok(),
            "abort must cancel the pending fetch, not hang"
        );
    }
}
