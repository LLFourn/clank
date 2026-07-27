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
/// A classified wake: the item plus its ACTION KEY — the normalized,
/// content-derived identity the dual-path coordinator dedups on. The
/// key is computed HERE, at classification time, because the raw
/// identities it needs (comment/review ids, the PR head sha, the push
/// ref+sha) live in the payload and are discarded by `WaitItem`
/// (codex e89be23). Both payload forms (events feed / webhook) yield
/// the SAME key for the same underlying action.
#[derive(Debug)]
pub(crate) struct Classified {
    pub(crate) item: WaitItem,
    /// `None` when a REQUIRED identity was missing from the payload:
    /// the coordinator then never dedups this wake (emit-always is the
    /// safe degradation) instead of collapsing unrelated actions into
    /// a manufactured shared key (codex bf1e1ea).
    pub(crate) key: Option<String>,
    /// The `/events` feed id — TRANSPORT metadata stamped by
    /// [`map_page`], not by the classifiers (webhook payloads have no
    /// feed id, and the classifiers must stay payload-symmetric). The
    /// WAL's durable cursor rides on it (github-offline-catchup).
    pub(crate) feed_id: Option<String>,
    /// The event's github-side time (epoch), best effort from the
    /// payload — the feed's `created_at`, or a per-kind webhook
    /// timestamp. `None` when the payload carried nothing usable;
    /// timeline surfaces fall back to ingest time
    /// (log-timeline-github-events).
    pub(crate) event_at: Option<u64>,
}

/// Epoch seconds from an RFC3339 timestamp (the shapes github emits:
/// `Z`-suffixed, numeric offsets, optional fractional seconds).
/// Externally supplied feed/webhook data: anything malformed —
/// impossible dates, out-of-range offsets — is `None` (fall back to
/// ingest time), never a plausible-but-wrong epoch that silently
/// reorders the timeline (codex 6219148). The `time` crate is already
/// a dependency; its RFC3339 parser owns the validation.
fn parse_iso8601_epoch(s: &str) -> Option<u64> {
    let t = time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339).ok()?;
    u64::try_from(t.unix_timestamp()).ok()
}

/// Best-effort event time from a WEBHOOK payload: the per-kind
/// timestamps the common shapes carry. Absent → `None`, and the
/// timeline falls back to ingest time.
fn webhook_event_at(payload: &serde_json::Value) -> Option<u64> {
    let paths: [&[&str]; 3] = [
        &["comment", "created_at"],
        &["review", "submitted_at"],
        &["head_commit", "timestamp"],
    ];
    for path in paths {
        let mut v = Some(payload);
        for k in path {
            v = v.and_then(|x| x.get(k));
        }
        if let Some(t) = v.and_then(|x| x.as_str()).and_then(parse_iso8601_epoch) {
            return Some(t);
        }
    }
    None
}

pub(crate) fn classify_event(
    repo: &str,
    event: &serde_json::Value,
    wanted: &[GithubEventKind],
    own_login: Option<&str>,
    include_own: bool,
    branches: &[String],
) -> Option<Classified> {
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
    let event_at = event
        .get("created_at")
        .and_then(|t| t.as_str())
        .and_then(parse_iso8601_epoch);
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
    // `branch_push` builds its item from the push payload (no
    // issue/PR object), so it short-circuits before the tuple match.
    // BRANCH refs only: PushEvent also fires for tag pushes, and
    // `refs/tags/*` is not a branch push — the strip_prefix gate is
    // the event-domain boundary, and the branches filter sees only
    // real short branch names (codex 3f04e3d).
    if ty == "PushEvent" {
        if !wanted.contains(&GithubEventKind::BranchPush) {
            return None;
        }
        let full_ref = payload.get("ref").and_then(|r| r.as_str())?;
        let branch = full_ref.strip_prefix("refs/heads/")?;
        if !branches.is_empty() && !branches.iter().any(|b| b == branch) {
            return None;
        }
        let size = payload.get("size").and_then(|s| s.as_u64()).unwrap_or(0);
        let url = payload
            .get("before")
            .and_then(|b| b.as_str())
            .zip(payload.get("head").and_then(|h| h.as_str()))
            .map(|(before, head)| format!("https://github.com/{repo}/compare/{before}...{head}"));
        let head = payload.get("head").and_then(|h| h.as_str());
        return Some(Classified {
            item: WaitItem::GithubEvent {
                repo: repo.to_string(),
                event: kind_str(GithubEventKind::BranchPush).to_string(),
                detail: None,
                number: None,
                title: Some(format!("{branch} +{size}")),
                actor: actor.map(str::to_string),
                url,
            },
            key: head.map(|h| format!("push@{full_ref}+{h}")),
            feed_id: None,
            event_at,
        });
    }

    let (kind, detail, obj): (GithubEventKind, Option<&str>, &serde_json::Value) = match ty {
        "PullRequestEvent" => {
            let merged = pr().get("merged").and_then(|m| m.as_bool()) == Some(true);
            match action {
                Some("opened") => (GithubEventKind::PrOpened, None, pr()),
                // New commits on the PR (the events feed's action for
                // a head update).
                Some("synchronize") => (GithubEventKind::PrUpdated, None, pr()),
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
    let key = action_key(kind, detail, payload, obj);
    Some(Classified {
        item: WaitItem::GithubEvent {
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
        },
        key,
        feed_id: None,
        event_at,
    })
}

/// The normalized action identity for the non-push kinds — SHARED by
/// both classifiers so the two payload forms cannot derive different
/// keys for one action. Feed event ids and webhook delivery GUIDs are
/// different id spaces and deliberately not used; the key is built
/// from the objects both forms carry.
fn action_key(
    kind: GithubEventKind,
    detail: Option<&str>,
    payload: &serde_json::Value,
    obj: &serde_json::Value,
) -> Option<String> {
    // Every identity is REQUIRED: a missing number/id/sha yields None
    // (the coordinator never dedups such a wake) — a manufactured `0`
    // or `?` would collapse unrelated actions into one key
    // (codex bf1e1ea).
    let number = obj.get("number").and_then(|n| n.as_u64());
    match kind {
        GithubEventKind::PrOpened => Some(format!("pr_opened#{}", number?)),
        GithubEventKind::PrMerged => Some(format!("pr_merged#{}", number?)),
        GithubEventKind::PrUpdated => {
            let head = payload
                .get("pull_request")
                .and_then(|p| p.get("head"))
                .and_then(|h| h.get("sha"))
                .and_then(|s| s.as_str())?;
            Some(format!("pr_updated#{}@{head}", number?))
        }
        GithubEventKind::IssueOpened => Some(format!("issue_opened#{}", number?)),
        GithubEventKind::IssueClosed => Some(format!("issue_closed#{}", number?)),
        // Comment-family ids live in DIFFERENT resource domains
        // (issue comments, review comments, reviews) — the key is
        // namespaced by the classified subtype so equal numeric ids
        // across families never collide (codex bf1e1ea).
        GithubEventKind::PrComment | GithubEventKind::IssueComment => {
            let (ns, container) = match detail {
                Some("review") => ("review", "review"),
                Some("review_comment") => ("review_comment", "comment"),
                Some("issue_comment_on_pr") => ("pr_issue_comment", "comment"),
                _ => ("issue_comment", "comment"),
            };
            let id = payload
                .get(container)
                .and_then(|c| c.get("id"))
                .and_then(|i| i.as_u64())?;
            Some(format!("{ns}#{id}"))
        }
        GithubEventKind::BranchPush => unreachable!("push keys are built in the push arms"),
    }
}

fn kind_str(kind: GithubEventKind) -> &'static str {
    match kind {
        GithubEventKind::PrOpened => "pr_opened",
        GithubEventKind::PrUpdated => "pr_updated",
        GithubEventKind::BranchPush => "branch_push",
        GithubEventKind::PrMerged => "pr_merged",
        GithubEventKind::PrComment => "pr_comment",
        GithubEventKind::IssueOpened => "issue_opened",
        GithubEventKind::IssueClosed => "issue_closed",
        GithubEventKind::IssueComment => "issue_comment",
    }
}

/// Classify one WEBHOOK delivery (github-http-client-and-realtime
/// M2). The relay hands us standard webhook payloads, which differ
/// from the events feed: the event NAME arrives out-of-band (the
/// `X-GitHub-Event` header), the action vocabulary is the webhook one
/// (`submitted` reviews, `synchronize`), the actor is `sender.login`,
/// and a push carries `after`/`ref` with a commits array. Same wanted/
/// own-actor/branches semantics as [`classify_event`]; tag pushes are
/// ignored by the same `refs/heads/` gate.
pub(crate) fn classify_webhook(
    repo: &str,
    event_name: &str,
    payload: &serde_json::Value,
    wanted: &[GithubEventKind],
    own_login: Option<&str>,
    include_own: bool,
    branches: &[String],
) -> Option<Classified> {
    let event_at = webhook_event_at(payload);
    let actor = payload
        .get("sender")
        .and_then(|a| a.get("login"))
        .and_then(|l| l.as_str());
    if !include_own
        && let (Some(own), Some(actor)) = (own_login, actor)
        && own == actor
    {
        return None;
    }
    let action = payload.get("action").and_then(|a| a.as_str());
    let pr = || {
        payload
            .get("pull_request")
            .unwrap_or(&serde_json::Value::Null)
    };

    if event_name == "push" {
        if !wanted.contains(&GithubEventKind::BranchPush) {
            return None;
        }
        let full_ref = payload.get("ref").and_then(|r| r.as_str())?;
        let branch = full_ref.strip_prefix("refs/heads/")?;
        if !branches.is_empty() && !branches.iter().any(|b| b == branch) {
            return None;
        }
        let size = payload
            .get("commits")
            .and_then(|c| c.as_array())
            .map(|c| c.len())
            .unwrap_or(0);
        let url = payload
            .get("before")
            .and_then(|b| b.as_str())
            .zip(payload.get("after").and_then(|h| h.as_str()))
            .map(|(before, after)| format!("https://github.com/{repo}/compare/{before}...{after}"));
        let after = payload.get("after").and_then(|h| h.as_str());
        return Some(Classified {
            item: WaitItem::GithubEvent {
                repo: repo.to_string(),
                event: kind_str(GithubEventKind::BranchPush).to_string(),
                detail: None,
                number: None,
                title: Some(format!("{branch} +{size}")),
                actor: actor.map(str::to_string),
                url,
            },
            key: after.map(|a| format!("push@{full_ref}+{a}")),
            feed_id: None,
            event_at,
        });
    }

    let (kind, detail, obj): (GithubEventKind, Option<&str>, &serde_json::Value) = match event_name
    {
        "pull_request" => {
            let merged = pr().get("merged").and_then(|m| m.as_bool()) == Some(true);
            match action {
                Some("opened") => (GithubEventKind::PrOpened, None, pr()),
                Some("synchronize") => (GithubEventKind::PrUpdated, None, pr()),
                Some("closed") if merged => (GithubEventKind::PrMerged, None, pr()),
                _ => return None,
            }
        }
        "issues" => {
            let issue = payload.get("issue").unwrap_or(&serde_json::Value::Null);
            match action {
                Some("opened") => (GithubEventKind::IssueOpened, None, issue),
                Some("closed") => (GithubEventKind::IssueClosed, None, issue),
                _ => return None,
            }
        }
        "issue_comment" if action == Some("created") => {
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
        // Webhook vocabulary: a new review is SUBMITTED.
        "pull_request_review" if action == Some("submitted") => {
            (GithubEventKind::PrComment, Some("review"), pr())
        }
        "pull_request_review_comment" if action == Some("created") => {
            (GithubEventKind::PrComment, Some("review_comment"), pr())
        }
        _ => return None,
    };
    if !wanted.contains(&kind) {
        return None;
    }
    let key = action_key(kind, detail, payload, obj);
    Some(Classified {
        item: WaitItem::GithubEvent {
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
        },
        key,
        feed_id: None,
        event_at,
    })
}

/// New items from one page (ids not in `seen`, matching + filtered),
/// plus the ids observed on the page. Pure.
fn map_page(
    src: &GithubSource,
    events: &[serde_json::Value],
    seen: &BTreeSet<String>,
    own_login: Option<&str>,
) -> (Vec<Classified>, Vec<String>) {
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
        if let Some(mut classified) = classify_event(
            &src.repo,
            event,
            &src.events,
            own_login,
            src.include_own_actions,
            &src.branches,
        ) {
            classified.feed_id = Some(id.to_string());
            items.push(classified);
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
#[derive(Debug)]
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
        items: Vec<Classified>,
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

/// The GitHub token, reusing gh's credential without shelling gh per
/// poll: `GH_TOKEN` / `GITHUB_TOKEN` env first (the standard
/// overrides gh itself honors), else ONE `gh auth token` subprocess —
/// the last place gh is invoked on the polling path
/// (github-http-client-and-realtime M1).
async fn acquire_github_token() -> anyhow::Result<String> {
    if let Some(t) = env_token(|k| std::env::var(k).ok()) {
        return Ok(t);
    }
    let out = tokio::process::Command::new("gh")
        .args(["auth", "token"])
        .kill_on_drop(true)
        .output()
        .await?;
    if !out.status.success() {
        anyhow::bail!(
            "no GH_TOKEN/GITHUB_TOKEN and `gh auth token` failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let token = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if token.is_empty() {
        anyhow::bail!("`gh auth token` returned nothing");
    }
    Ok(token)
}

/// Env-token precedence, injected for tests: GH_TOKEN beats
/// GITHUB_TOKEN, blanks don't count.
fn env_token(get: impl Fn(&str) -> Option<String>) -> Option<String> {
    ["GH_TOKEN", "GITHUB_TOKEN"]
        .iter()
        .filter_map(|k| get(k))
        .map(|t| t.trim().to_string())
        .find(|t| !t.is_empty())
}

/// A cached token that can be invalidated (a 401 means it rotated) and
/// re-acquired on the next use — acquisition injected for tests.
struct TokenCell(tokio::sync::Mutex<Option<String>>);

impl TokenCell {
    fn new() -> Self {
        Self(tokio::sync::Mutex::new(None))
    }
    async fn invalidate(&self) {
        *self.0.lock().await = None;
    }
    async fn get_or_acquire<F, Fut>(&self, acquire: F) -> anyhow::Result<String>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = anyhow::Result<String>>,
    {
        let mut slot = self.0.lock().await;
        if let Some(t) = slot.as_ref() {
            return Ok(t.clone());
        }
        let fresh = acquire().await?;
        *slot = Some(fresh.clone());
        Ok(fresh)
    }
}

/// A minimal response the session yields — our own shape so the
/// transport is injectable in tests without fabricating reqwest types.
pub(crate) struct MiniResponse {
    pub(crate) status: u16,
    pub(crate) headers: std::collections::BTreeMap<String, String>,
    pub(crate) body: String,
}

impl MiniResponse {
    fn header_u64(&self, name: &str) -> Option<u64> {
        self.headers.get(name)?.parse().ok()
    }
}

/// The raw HTTP GET, injected so the session's auth behavior is
/// testable without a network.
pub(crate) trait HttpSend: Send + Sync {
    fn send(
        &self,
        url: &str,
        bearer: &str,
        etag: Option<&str>,
    ) -> impl std::future::Future<Output = anyhow::Result<MiniResponse>> + Send;
}

/// Production sender: reqwest (rustls), typed statuses and headers —
/// the gh-CLI text parsing this transport replaced produced the
/// 304-noise/CRLF/substring-status bug class.
pub(crate) struct ReqwestSend(reqwest::Client);

impl HttpSend for ReqwestSend {
    async fn send(
        &self,
        url: &str,
        bearer: &str,
        etag: Option<&str>,
    ) -> anyhow::Result<MiniResponse> {
        let mut req = self
            .0
            .get(url)
            .bearer_auth(bearer)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28");
        if let Some(tag) = etag {
            req = req.header("If-None-Match", tag);
        }
        let resp = req.send().await?;
        let status = resp.status().as_u16();
        let headers = resp
            .headers()
            .iter()
            .filter_map(|(k, v)| {
                Some((
                    k.as_str().to_ascii_lowercase(),
                    v.to_str().ok()?.to_string(),
                ))
            })
            .collect();
        let body = resp.text().await.unwrap_or_default();
        Ok(MiniResponse {
            status,
            headers,
            body,
        })
    }
}

/// A token acquirer: the boxed-future fn shape both production
/// (`acquire_github_token`) and the tests' scripted acquirers share.
type AcquireFn =
    fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<String>> + Send>>;

/// ONE host-scoped authenticated session shared by every github
/// source in the process (codex f9807c3: per-source cells shelled gh
/// once per source and let a rotated token gate /user forever). Every
/// endpoint goes through [`GithubSession::get`], which has the ONE
/// uniform 401 story: invalidate, re-acquire, retry ONCE; a second
/// 401 is the error.
pub(crate) struct GithubSession<S> {
    http: S,
    token: TokenCell,
    acquire: AcquireFn,
}

/// The github.com session, created on first use (host-scoped: one per
/// process today; a hosts map when enterprise arrives).
pub(crate) fn shared_session() -> std::sync::Arc<GithubSession<ReqwestSend>> {
    static SESSION: std::sync::OnceLock<std::sync::Arc<GithubSession<ReqwestSend>>> =
        std::sync::OnceLock::new();
    SESSION
        .get_or_init(|| {
            std::sync::Arc::new(GithubSession {
                http: ReqwestSend(
                    reqwest::Client::builder()
                        .user_agent("clank")
                        .build()
                        .expect("static client config"),
                ),
                token: TokenCell::new(),
                acquire: || Box::pin(acquire_github_token()),
            })
        })
        .clone()
}

impl<S: HttpSend> GithubSession<S> {
    #[cfg(test)]
    fn for_tests(
        http: S,
        acquire: fn() -> std::pin::Pin<
            Box<dyn std::future::Future<Output = anyhow::Result<String>> + Send>,
        >,
    ) -> Self {
        Self {
            http,
            token: TokenCell::new(),
            acquire,
        }
    }

    /// Authorized GET with the uniform 401 handling.
    async fn get(&self, url: &str, etag: Option<&str>) -> anyhow::Result<MiniResponse> {
        let token = self.token.get_or_acquire(self.acquire).await?;
        let resp = self.http.send(url, &token, etag).await?;
        if resp.status != 401 {
            return Ok(resp);
        }
        // The token rotated: re-acquire and retry exactly once.
        self.token.invalidate().await;
        let token = self.token.get_or_acquire(self.acquire).await?;
        let resp = self.http.send(url, &token, etag).await?;
        if resp.status == 401 {
            anyhow::bail!("github {url}: 401 after token re-acquisition");
        }
        Ok(resp)
    }
}

/// Production fetcher: the events endpoint over the SHARED session.
struct GhFetcher<S: HttpSend> {
    repo: String,
    session: std::sync::Arc<GithubSession<S>>,
}

impl<S: HttpSend> EventFetcher for GhFetcher<S> {
    async fn fetch(&self, page: u32, etag: Option<String>) -> anyhow::Result<GhResponse> {
        let url = format!(
            "https://api.github.com/repos/{}/events?per_page=100&page={page}",
            self.repo
        );
        let resp = self.session.get(&url, etag.as_deref()).await?;
        match resp.status {
            304 => Ok(GhResponse::NotModified {
                poll_interval: resp.header_u64("x-poll-interval"),
            }),
            s if (200..300).contains(&s) => {
                let events: Vec<serde_json::Value> = serde_json::from_str(&resp.body)?;
                Ok(GhResponse::Ok {
                    etag: resp.headers.get("etag").cloned(),
                    poll_interval: resp.header_u64("x-poll-interval"),
                    events,
                })
            }
            s => anyhow::bail!(
                "github /events: {s}: {}",
                resp.body.chars().take(200).collect::<String>()
            ),
        }
    }
}

/// The authenticated login for the own-action filter — `GET /user`
/// over the SAME session and 401 story as the events endpoint.
async fn fetch_authenticated_login<S: HttpSend>(
    session: &GithubSession<S>,
) -> anyhow::Result<String> {
    let resp = session.get("https://api.github.com/user", None).await?;
    if !(200..300).contains(&resp.status) {
        anyhow::bail!("github /user: {}", resp.status);
    }
    let v: serde_json::Value = serde_json::from_str(&resp.body)?;
    v.get("login")
        .and_then(|l| l.as_str())
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("github /user returned no login"))
}

/// Resolves the authenticated login for the own-action filter —
/// injected so a login failure-then-success is testable.
pub(crate) trait LoginResolver {
    fn resolve(&self) -> impl Future<Output = anyhow::Result<String>> + Send;
}

/// Production resolver: `GET /user` over the shared session.
struct GhLogin<S: HttpSend>(std::sync::Arc<GithubSession<S>>);
impl<S: HttpSend> LoginResolver for GhLogin<S> {
    async fn resolve(&self) -> anyhow::Result<String> {
        fetch_authenticated_login(&self.0).await
    }
}

/// Extract one relay delivery from a webhook request: the event name
/// from `X-GitHub-Event`, the payload from the JSON body. Generic
/// over the body so tests drive it with fabricated requests.
/// One loopback request may not buffer unboundedly: the count-bounded
/// inbox doesn't bound MEMORY if a single body is huge (codex
/// 4d77b83). Real webhook payloads are kilobytes; a MiB is generous.
const MAX_WEBHOOK_BODY: usize = 1 << 20;

async fn webhook_from_request<B>(req: hyper::Request<B>) -> Option<RawDelivery>
where
    B: hyper::body::Body,
    B::Data: Send,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    use http_body_util::BodyExt;
    let name = req
        .headers()
        .get("x-github-event")?
        .to_str()
        .ok()?
        .to_string();
    let limited = http_body_util::Limited::new(req.into_body(), MAX_WEBHOOK_BODY);
    let bytes = limited.collect().await.ok()?.to_bytes();
    let payload = serde_json::from_slice(&bytes).ok()?;
    Some((name, payload))
}

/// Serve the loopback listener: each delivery lands in the inbox
/// (ingress-bounded), 200 back to the forwarder. Connections are
/// served INLINE — no detached per-connection tasks, so aborting the
/// supervisor cancels everything it owns.
async fn run_relay_listener(listener: tokio::net::TcpListener, inbox: std::sync::Arc<RelayInbox>) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            break;
        };
        let io = hyper_util::rt::TokioIo::new(stream);
        let inbox = inbox.clone();
        let svc = hyper::service::service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
            let inbox = inbox.clone();
            async move {
                if let Some(raw) = webhook_from_request(req).await {
                    inbox.push(raw);
                }
                Ok::<_, std::convert::Infallible>(hyper::Response::new(http_body_util::Empty::<
                    hyper::body::Bytes,
                >::new()))
            }
        });
        let _ = hyper::server::conn::http1::Builder::new()
            .serve_connection(io, svc)
            .await;
    }
}

/// The webhook event names a source's sub-kinds subscribe to — what
/// `gh webhook forward --events` receives.
fn webhook_event_names(events: &[GithubEventKind]) -> Vec<&'static str> {
    let mut names = Vec::new();
    for k in events {
        let name = match k {
            GithubEventKind::PrOpened | GithubEventKind::PrUpdated | GithubEventKind::PrMerged => {
                "pull_request"
            }
            GithubEventKind::PrComment => "pull_request_review", // + comment kinds below
            GithubEventKind::IssueOpened | GithubEventKind::IssueClosed => "issues",
            GithubEventKind::IssueComment => "issue_comment",
            GithubEventKind::BranchPush => "push",
        };
        if !names.contains(&name) {
            names.push(name);
        }
    }
    // pr_comment spans three webhook events.
    if events.contains(&GithubEventKind::PrComment) {
        for extra in ["pull_request_review_comment", "issue_comment"] {
            if !names.contains(&extra) {
                names.push(extra);
            }
        }
    }
    names
}

/// The forwarder's argv — pure, pinned by test (codex 4d77b83).
fn forwarder_argv(repo: &str, events: &str, port: u16) -> Vec<String> {
    vec![
        "webhook".into(),
        "forward".into(),
        format!("--repo={repo}"),
        format!("--events={events}"),
        format!("--url=http://127.0.0.1:{port}"),
    ]
}

/// The forwarder process seam: run until TERMINAL, returning the one
/// bounded human reason (spawn failure, or exit status + last stderr
/// line). Injected so the supervisor's outcomes are scriptable
/// without gh (codex 4d77b83).
pub(crate) trait Forwarder: Send + Sync + 'static {
    fn run(
        &self,
        repo: String,
        events: String,
        port: u16,
    ) -> impl std::future::Future<Output = String> + Send;
}

/// Production forwarder: `gh webhook forward` in its own process
/// group, killed with the supervisor (kill_on_drop + group kill);
/// stderr captured so the terminal reason is USEFUL (a missing
/// extension or admin refusal names itself).
struct GhForwarder;

impl Forwarder for GhForwarder {
    async fn run(&self, repo: String, events: String, port: u16) -> String {
        let mut cmd = tokio::process::Command::new("gh");
        cmd.args(forwarder_argv(&repo, &events, port))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .process_group(0)
            .kill_on_drop(true);
        run_forwarder_process(cmd).await
    }
}

/// Supervise a spawned forwarder-shaped command to its terminal
/// reason. stderr is drained CONCURRENTLY with the wait into a
/// bounded tail — a chatty extension that fills the OS pipe would
/// otherwise block before exit and the wait would never complete
/// (codex 24c3355); the post-exit drain uses the same absolute
/// deadline discipline as command sources.
async fn run_forwarder_process(mut cmd: tokio::process::Command) -> String {
    use tokio::io::AsyncReadExt;
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return format!("failed to start: {e}"),
    };
    let _group = crate::cli::wait::GroupKill(child.id());
    let mut stderr = child.stderr.take();
    let mut tail = crate::cli::wait::RingTail::new(4096);
    let mut buf = [0u8; 1024];
    let status = loop {
        tokio::select! {
            st = child.wait() => break st,
            r = async { stderr.as_mut().unwrap().read(&mut buf).await }, if stderr.is_some() => {
                match r {
                    Ok(0) | Err(_) => stderr = None,
                    Ok(n) => tail.push(&buf[..n]),
                }
            }
        }
    };
    // Catch a final line written just before exit — bounded both ways.
    let _ = tokio::time::timeout(std::time::Duration::from_millis(100), async {
        while let Some(pipe) = stderr.as_mut() {
            match tokio::time::timeout(std::time::Duration::from_millis(20), pipe.read(&mut buf))
                .await
            {
                Ok(Ok(n)) if n > 0 => tail.push(&buf[..n]),
                _ => break,
            }
        }
    })
    .await;
    let text = tail.into_string();
    let last_line = text.lines().last().unwrap_or("").to_string();
    match status {
        Ok(st) => format!("forwarder exited {st}: {last_line}"),
        Err(e) => format!("forwarder wait failed: {e}"),
    }
}

/// Aborts the relay's listener/supervisor tasks when the source dies —
/// the forwarder child itself dies by kill_on_drop + group kill inside
/// the supervisor task.
struct RelayGuard(Vec<tokio::task::JoinHandle<()>>);
impl Drop for RelayGuard {
    fn drop(&mut self) {
        for h in &self.0 {
            h.abort();
        }
    }
}

/// Start the realtime relay: a loopback-only listener (by
/// construction — bound to 127.0.0.1) plus a supervised
/// `gh webhook forward` child in its own process group. Any failure
/// to start, and the forwarder EXITING, close the inbox — the poll
/// loop logs the fallback once and continues on polling alone
/// (polling never stopped; nothing is lost). Requires ADMIN on the
/// watched repo and the cli/gh-webhook extension; GitHub bills the
/// relay as dev tooling — all stated in the skill docs.
async fn start_relay<Fw: Forwarder>(
    src: &GithubSource,
    forwarder: Fw,
) -> Option<(std::sync::Arc<RelayInbox>, RelayGuard)> {
    let listener = match tokio::net::TcpListener::bind(("127.0.0.1", 0)).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!(
                "wait: github {} — realtime listener failed to bind ({e}); polling alone",
                src.repo
            );
            return None;
        }
    };
    let port = listener.local_addr().ok()?.port();
    let inbox = std::sync::Arc::new(RelayInbox::new());
    let listen_task = tokio::spawn(run_relay_listener(listener, inbox.clone()));
    let events = webhook_event_names(&src.events).join(",");
    let repo = src.repo.clone();
    let inbox_for_child = inbox.clone();
    let child_task = tokio::spawn(async move {
        // The supervisor prints NOTHING: the terminal reason lands in
        // the inbox and poll_loop's fallback line is the single
        // diagnostic owner (codex 4d77b83).
        let reason = forwarder.run(repo, events, port).await;
        inbox_for_child.close_with(&reason);
    });
    Some((inbox, RelayGuard(vec![listen_task, child_task])))
}

/// One github wake source: the production entry point. Every source
/// in the process shares ONE host-scoped session (one token
/// acquisition, one 401 story — codex f9807c3). `delivery: realtime`
/// adds the webhook relay as the latency path; polling remains the
/// completeness backstop either way.
pub(crate) async fn run_github_source(
    src: GithubSource,
    tx: tokio::sync::mpsc::UnboundedSender<WaitItem>,
    events_dir: Option<std::path::PathBuf>,
) {
    let session = shared_session();
    let fetcher = GhFetcher {
        repo: src.repo.clone(),
        session: session.clone(),
    };
    let log = events_dir.and_then(|dir| {
        match crate::cli::github_event_log::EventLog::open(&dir, &source_key(&src)) {
            Ok(l) => Some(l),
            Err(e) => {
                eprintln!(
                    "wait: github {} — can't open the event log dir ({e}); \
                     continuing without offline catch-up",
                    src.repo
                );
                None
            }
        }
    });
    let relay = if src.delivery == clank_core::agent_config::Delivery::Realtime {
        start_relay(&src, GhForwarder).await
    } else {
        None
    };
    let inbox = relay.as_ref().map(|(i, _)| i.clone());
    poll_loop(&src, &GhLogin(session), &fetcher, &tx, inbox, log).await;
    // RelayGuard drops here (and on task abort): listener + supervisor
    // die with the source; the forwarder child by group kill.
}

/// Stable per-source WAL key: the sanitized repo plus a short hash of
/// the source's full identity. Different kinds/branches/own-action
/// settings classify differently — a different presentable set is a
/// DIFFERENT log (a config edit re-baselines rather than misreading
/// an old cursor). FNV-1a inlined for cross-release stability, like
/// the zellij session hash.
fn source_key(src: &GithubSource) -> String {
    let slug: String = src
        .repo
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let identity = format!(
        "{}|{:?}|{:?}|{}",
        src.repo, src.events, src.branches, src.include_own_actions
    );
    let mut h: u32 = 0x811c9dc5;
    for b in identity.as_bytes() {
        h ^= u32::from(*b);
        h = h.wrapping_mul(0x0100_0193);
    }
    format!("github-{slug}-{h:08x}")
}

/// The per-source EMISSION gate — one owner for what reaches the wait
/// loop across both delivery paths (the coordinator model, codex
/// 1ddc0f6/afb7a35). First arrival with a given action key emits; the
/// other path's copy drops at the gate. A KEYLESS wake (missing
/// payload identity) is never deduped — emit-always is the safe
/// degradation. The poll BASELINE owns the horizon: it seeds the key
/// set and emits nothing, and relay deliveries BUFFER (bounded,
/// drop-oldest) until a baseline has succeeded, then drain through
/// the gate — realtime without a poll-established horizon could
/// replay history.
pub(crate) struct Coordinator {
    keys: SeenSet,
    baseline_done: bool,
    buffered: std::collections::VecDeque<Classified>,
    tx: tokio::sync::mpsc::UnboundedSender<WaitItem>,
    /// The write-ahead sink: every gate decision lands here BEFORE any
    /// emission (github-offline-catchup). Disabled for sources without
    /// a persistence dir.
    wal: crate::cli::github_event_log::SharedWal,
}

/// Pre-baseline relay backlog bound — startup-window sized. The
/// buffer is memory-only by design: nothing here has been logged yet
/// (its durable decision happens at the gate when the baseline
/// drains), and the poll backstop re-observes lost actions.
const RELAY_BUFFER_CAP: usize = 256;

impl Coordinator {
    /// Test convenience: a coordinator with no persistence.
    #[cfg(test)]
    pub(crate) fn new(tx: tokio::sync::mpsc::UnboundedSender<WaitItem>) -> Self {
        Self::with_wal(tx, crate::cli::github_event_log::SharedWal::disabled())
    }

    pub(crate) fn with_wal(
        tx: tokio::sync::mpsc::UnboundedSender<WaitItem>,
        wal: crate::cli::github_event_log::SharedWal,
    ) -> Self {
        Self {
            keys: SeenSet::new(SEEN_CAP),
            baseline_done: false,
            buffered: std::collections::VecDeque::new(),
            tx,
            wal,
        }
    }

    /// Warm start from a persisted log: the action-key horizon is
    /// already durable, so relay deliveries need no baseline buffering
    /// — a replay of a pre-restart action drops at the gate instead of
    /// double-emitting (github-offline-catchup).
    pub(crate) fn warm(&mut self, keys: impl IntoIterator<Item = String>) {
        self.keys.extend(keys);
        self.baseline_done = true;
    }

    /// The one emission gate for BOTH paths: decides, writes the
    /// event's single atomic WAL record (inbox when emitting, obs when
    /// the other path already emitted the action), THEN emits. Returns
    /// false when the wait side is gone (the caller should stop).
    fn gate(&mut self, c: Classified, transport: crate::cli::github_event_log::Transport) -> bool {
        // Keyless wakes skip the dedup entirely: never dedup.
        if let Some(k) = &c.key {
            if self.keys.as_btree().contains(k) {
                // The other path emitted this action: this copy's
                // durable decision is an observation.
                self.wal.obs(c.feed_id.as_deref(), Some(k), false);
                return true;
            }
            self.keys.extend([k.clone()]);
        }
        self.wal.inbox(
            transport,
            c.feed_id.as_deref(),
            c.key.as_deref(),
            c.event_at,
            &c.item,
        );
        self.tx.send(c.item).is_ok()
    }

    /// A classified wake from the POLL path.
    pub(crate) fn offer(&mut self, c: Classified) -> bool {
        self.gate(c, crate::cli::github_event_log::Transport::Poll)
    }

    /// The BASELINE poll's actions: seed the gate, emit nothing (each
    /// action's durable record is a baseline observation), then drain
    /// any pre-baseline relay backlog through the gate (pre-seeded
    /// keys drop; genuinely-new actions emit once).
    pub(crate) fn baseline(&mut self, baseline: Vec<Classified>) -> bool {
        for c in baseline {
            self.wal.obs(c.feed_id.as_deref(), c.key.as_deref(), true);
            self.keys.extend(c.key);
        }
        self.baseline_done = true;
        while let Some(c) = self.buffered.pop_front() {
            if !self.gate(c, crate::cli::github_event_log::Transport::Relay) {
                return false;
            }
        }
        true
    }

    /// A relay delivery: buffered until the baseline succeeds, gated
    /// afterward.
    pub(crate) fn deliver(&mut self, c: Classified) -> bool {
        if !self.baseline_done {
            if self.buffered.len() == RELAY_BUFFER_CAP {
                self.buffered.pop_front();
                eprintln!("wait: github relay backlog overflow pre-baseline; oldest dropped");
            }
            self.buffered.push_back(c);
            return true;
        }
        self.gate(c, crate::cli::github_event_log::Transport::Relay)
    }
}

/// One relay delivery as the loopback listener hands it over: the
/// webhook event NAME (the `X-GitHub-Event` header) plus the payload.
/// RAW on purpose — classification happens at DRAIN time in the poll
/// loop, where `own_login` is resolved, so a delivery arriving before
/// identity resolution can never bypass the fail-closed own-actor
/// filter (codex c29a07c): it simply waits in the inbox.
pub(crate) type RawDelivery = (String, serde_json::Value);

/// The relay inbox: a BOUNDED drop-oldest queue whose capacity lives
/// at PRODUCER ingress (codex 840725c) — `push` itself evicts, so no
/// pending await anywhere in the consumer (a hung /user call, a slow
/// poll) can let deliveries accumulate beyond the cap. std Mutex (no
/// await while held) + Notify for the consumer wake.
pub(crate) struct RelayInbox {
    queue: std::sync::Mutex<std::collections::VecDeque<RawDelivery>>,
    notify: tokio::sync::Notify,
    closed: std::sync::atomic::AtomicBool,
    /// Why the relay ended — rendered by the ONE diagnostic owner
    /// (poll_loop's fallback line); bounded at write (codex 4d77b83).
    reason: std::sync::Mutex<Option<String>>,
}

impl RelayInbox {
    pub(crate) fn new() -> Self {
        Self {
            queue: std::sync::Mutex::new(std::collections::VecDeque::new()),
            notify: tokio::sync::Notify::new(),
            closed: std::sync::atomic::AtomicBool::new(false),
            reason: std::sync::Mutex::new(None),
        }
    }

    /// Enqueue, applying the overflow policy AT INGRESS: at capacity
    /// the oldest delivery is dropped (with a stderr note).
    pub(crate) fn push(&self, raw: RawDelivery) {
        {
            let mut q = self.queue.lock().unwrap();
            if q.len() == RELAY_BUFFER_CAP {
                q.pop_front();
                eprintln!("wait: github relay inbox overflow; oldest delivery dropped");
            }
            q.push_back(raw);
        }
        self.notify.notify_one();
    }

    /// The producer is gone: record WHY (bounded) and wake the
    /// consumer so it can fall back. The reason is rendered exactly
    /// once, by poll_loop's fallback line — no other diagnostic owner.
    pub(crate) fn close_with(&self, reason: &str) {
        let bounded: String = reason.chars().take(200).collect();
        *self.reason.lock().unwrap() = Some(bounded);
        self.closed.store(true, std::sync::atomic::Ordering::SeqCst);
        self.notify.notify_one();
    }

    #[cfg(test)]
    pub(crate) fn close(&self) {
        self.close_with("closed");
    }

    pub(crate) fn reason(&self) -> String {
        self.reason
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| "relay ended".to_string())
    }

    /// Next delivery, or `None` once closed AND drained. Cancellation-
    /// safe: the check-then-wait loop re-checks after every wake, and
    /// `notify_one` stores a permit when nobody is waiting.
    pub(crate) async fn recv(&self) -> Option<RawDelivery> {
        loop {
            if let Some(raw) = self.queue.lock().unwrap().pop_front() {
                return Some(raw);
            }
            if self.closed.load(std::sync::atomic::Ordering::SeqCst) {
                return None;
            }
            self.notify.notified().await;
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.queue.lock().unwrap().len()
    }
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
    mut relay: Option<std::sync::Arc<RelayInbox>>,
    mut log: Option<crate::cli::github_event_log::EventLog>,
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

    // Warm start (github-offline-catchup): rebuild both horizons from
    // the WAL and re-present the unhandled backlog IMMEDIATELY — an
    // agent arming with unhandled events is woken for them before any
    // network I/O (and before the own-login gate: the backlog was
    // already filtered when it was logged). A warm cursor makes the
    // first fetch a RECONCILE — events that fired while no wait was
    // armed emit instead of being swallowed by a fresh baseline.
    let mut warm_keys: Vec<String> = Vec::new();
    let mut warm = false;
    if let Some(l) = log.as_mut() {
        let loaded = l.load();
        if loaded.log_quarantined {
            eprintln!(
                "wait: github {} — event log was corrupt; quarantined to .corrupt and \
                 re-baselining (unhandled events, if any, were lost)",
                src.repo
            );
        }
        if loaded.state_corrupt {
            eprintln!(
                "wait: github {} — event cache state was corrupt; resetting only the \
                 etag/poll-interval cache",
                src.repo
            );
        }
        if loaded.foreign > 0 {
            eprintln!(
                "wait: github {} — {} event-log record(s) from another clank version \
                 preserved but skipped",
                src.repo, loaded.foreign
            );
        }
        for entry in &loaded.unhandled {
            if tx.send(entry.item.clone()).is_err() {
                return; // wait gone
            }
        }
        seen.extend(loaded.feed_ids);
        // Cache metadata is SUBORDINATE to a valid WAL horizon (codex
        // 7c9e7fe): a cold start (absent or quarantined log) must
        // fetch UNCONDITIONALLY — a stale ETag from a surviving
        // state.json could 304 forever, never establishing the
        // baseline and leaving pre-baseline relay deliveries buffered
        // for good.
        if loaded.warm {
            etag = loaded.etag;
            floor = loaded.poll_interval_floor;
        }
        warm_keys = loaded.action_keys;
        warm = loaded.warm;
    }
    let wal = crate::cli::github_event_log::SharedWal::new(log);
    let mut coordinator = Coordinator::with_wal(tx.clone(), wal.clone());
    if warm {
        coordinator.warm(warm_keys);
        established = true;
    }

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
                    // The relay inbox is bounded at INGRESS, so
                    // deliveries simply wait in it while we retry —
                    // no drain needed here (codex 840725c).
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
            Ok(PollDelta::NotModified) => {
                // Nothing new, but the floor may have moved.
                wal.save_state(etag.as_deref(), floor);
            }
            Ok(PollDelta::Fetched {
                items,
                new_ids,
                overrun,
            }) => {
                // Pagination can shift events across pages mid-tick, so
                // the SAME presentable event can appear in `items`
                // twice. One event = one gate decision = one primary
                // record (codex 7c9e7fe): keep each feed id's first
                // occurrence only (keyless items would otherwise
                // double-emit; keyed ones would append inbox + obs).
                let items = {
                    let mut seen_ids: std::collections::HashSet<String> =
                        std::collections::HashSet::new();
                    let mut deduped = Vec::with_capacity(items.len());
                    for c in items {
                        match &c.feed_id {
                            Some(id) if !seen_ids.insert(id.clone()) => {}
                            _ => deduped.push(c),
                        }
                    }
                    deduped
                };
                // Durable observations for newly observed ids with no
                // presentable item — exactly one atomic primary record
                // per event: presentable ids get their inbox record at
                // the emission gate instead (codex df3f1a7). Computed
                // against the PRE-tick cursor; pages can repeat an id
                // within one tick, so dedupe locally.
                {
                    let pre = seen.as_btree();
                    let presentable: std::collections::HashSet<&str> =
                        items.iter().filter_map(|c| c.feed_id.as_deref()).collect();
                    let mut recorded: std::collections::HashSet<&str> =
                        std::collections::HashSet::new();
                    for id in &new_ids {
                        if !pre.contains(id)
                            && !presentable.contains(id.as_str())
                            && recorded.insert(id)
                        {
                            wal.obs(Some(id), None, !established);
                        }
                    }
                }
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
                // count, so real history is never replayed as new. The
                // coordinator owns emission for BOTH paths: the
                // baseline seeds its key gate, delta polls offer
                // through it (a webhook that already emitted an action
                // suppresses the poll's copy, and vice versa).
                if established {
                    for c in items {
                        if !coordinator.offer(c) {
                            return; // wait gone
                        }
                    }
                } else if !coordinator.baseline(items) {
                    return; // wait gone
                }
                established = true;
                // A completed tick is the durability point for the
                // cache metadata; failed ticks touch nothing.
                wal.save_state(etag.as_deref(), floor);
                wal.compact();
            }
            Err(e) => {
                // outcome.etag is deliberately DROPPED here: the tick
                // didn't complete, so conditioning the next one with
                // its ETag would 304 straight past the unread deeper
                // pages' events. Retry the whole tick unconditioned.
                eprintln!("wait: github {} poll failed: {e}", src.repo);
            }
        }
        // Between ticks: the relay is the LATENCY path — deliveries
        // classify and emit IMMEDIATELY (through the same coordinator
        // gate) instead of waiting for the next poll. A closed relay
        // falls back to polling alone, loudly, once; polling never
        // stopped, so nothing is lost.
        let deadline = tokio::time::Instant::now() + effective_interval(configured, floor);
        loop {
            let Some(inbox) = relay.as_ref() else {
                tokio::time::sleep_until(deadline).await;
                break;
            };
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => break,
                delivery = inbox.recv() => match delivery {
                    Some((name, payload)) => {
                        if let Some(c) = classify_webhook(
                            &src.repo,
                            &name,
                            &payload,
                            &src.events,
                            own_login.as_deref(),
                            src.include_own_actions,
                            &src.branches,
                        ) && !coordinator.deliver(c)
                        {
                            return; // wait gone
                        }
                    }
                    None => {
                        // THE one fallback diagnostic (codex 4d77b83):
                        // the supervisor records why, this line renders
                        // it, nobody else prints.
                        eprintln!(
                            "wait: github {} — realtime relay ended ({}); \
                             continuing on polling alone",
                            src.repo,
                            relay.as_ref().map(|i| i.reason()).unwrap_or_default()
                        );
                        relay = None;
                    }
                },
            }
        }
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
            branches: Vec::new(),
            delivery: clank_core::agent_config::Delivery::Poll,
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
            let item = classify_event("o/r", &raw, all, None, false, &[])
                .unwrap_or_else(|| panic!("must classify {want}"))
                .item;
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
    fn branch_push_is_refs_heads_only_and_branch_filtered() {
        use GithubEventKind::BranchPush;
        let push = |r: &str| {
            ev(
                "1",
                "PushEvent",
                serde_json::json!({"ref": r, "size": 2,
                    "before": "aaaa", "head": "bbbb", "commits": []}),
                "x",
            )
        };
        // A TAG push is ignored even when branch_push is subscribed
        // with an empty branches filter (codex 3f04e3d).
        assert!(
            classify_event(
                "o/r",
                &push("refs/tags/v1.0"),
                &[BranchPush],
                None,
                false,
                &[]
            )
            .is_none(),
            "refs/tags/* is not a branch push"
        );
        // Unsubscribed configs drop branch pushes.
        assert!(
            classify_event(
                "o/r",
                &push("refs/heads/main"),
                &[GithubEventKind::PrOpened],
                None,
                false,
                &[]
            )
            .is_none()
        );
        // The branches filter admits by SHORT ref and drops others.
        let main_only = vec!["main".to_string()];
        assert!(
            classify_event(
                "o/r",
                &push("refs/heads/main"),
                &[BranchPush],
                None,
                false,
                &main_only
            )
            .is_some()
        );
        assert!(
            classify_event(
                "o/r",
                &push("refs/heads/dev"),
                &[BranchPush],
                None,
                false,
                &main_only
            )
            .is_none()
        );
        // Own-actor filtering applies like every other kind.
        assert!(
            classify_event(
                "o/r",
                &push("refs/heads/main"),
                &[BranchPush],
                Some("x"),
                false,
                &[]
            )
            .is_none(),
            "own push must not self-wake"
        );
    }

    #[test]
    fn pr_updated_maps_only_the_synchronize_action() {
        use GithubEventKind::PrUpdated;
        let pr_ev = |action: &str| {
            ev(
                "1",
                "PullRequestEvent",
                serde_json::json!({"action": action,
                    "pull_request": {"number": 7, "title": "t", "html_url": "u"}}),
                "x",
            )
        };
        let item = classify_event("o/r", &pr_ev("synchronize"), &[PrUpdated], None, false, &[])
            .expect("synchronize → pr_updated")
            .item;
        let WaitItem::GithubEvent { event, number, .. } = item else {
            unreachable!()
        };
        assert_eq!(event, "pr_updated");
        assert_eq!(number, Some(7));
        // Other head-adjacent actions stay unmapped.
        for action in ["edited", "labeled", "reopened"] {
            assert!(
                classify_event("o/r", &pr_ev(action), &[PrUpdated], None, false, &[]).is_none(),
                "{action} is not a pr_updated"
            );
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
            let item = classify_event("o/r", &raw, &[GithubEventKind::PrMerged], None, false, &[])
                .map(|c| c.item);
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
                classify_event("o/r", &raw, wanted, None, false, &[]).is_none(),
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
        assert!(
            classify_event("o/r", &raw, &[GithubEventKind::PrMerged], None, false, &[]).is_none()
        );
        let raw = ev(
            "1",
            "PullRequestEvent",
            serde_json::json!({"action":"opened","pull_request":{"number":1}}),
            "me",
        );
        assert!(
            classify_event(
                "o/r",
                &raw,
                &[GithubEventKind::PrOpened],
                Some("me"),
                false,
                &[]
            )
            .is_none()
        );
        assert!(
            classify_event(
                "o/r",
                &raw,
                &[GithubEventKind::PrOpened],
                Some("me"),
                true,
                &[]
            )
            .is_some()
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
                PrUpdated,
                PrMerged,
                PrComment,
                IssueOpened,
                IssueClosed,
                IssueComment,
                BranchPush,
            ],
            poll_interval: None,
            include_own_actions: true, // don't filter — assert full mapping
            branches: Vec::new(),
            delivery: clank_core::agent_config::Delivery::Poll,
        };
        let (items, _ids) = map_page(&s, &raw, &BTreeSet::new(), None);
        let got: Vec<(&str, Option<&str>, Option<u64>)> = items
            .iter()
            .map(|c| match &c.item {
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
                ("pr_updated", None, Some(12)),
                ("branch_push", None, None),
            ],
            "the WatchEvent AND the tag push are dropped; every other \
             kind maps with its detail"
        );
        // The branch push carries the short ref + size title and the
        // actionable compare URL from before/head.
        let WaitItem::GithubEvent { title, url, .. } = &items.last().unwrap().item else {
            unreachable!()
        };
        assert_eq!(title.as_deref(), Some("main +3"));
        assert_eq!(
            url.as_deref(),
            Some(
                "https://github.com/o/r/compare/883efe034920928c47fe18598c01249d1a9fdabd...\
                 7a8f3ac80e2ad2f6842cb86f576d4bfe2c03e300"
            )
        );
        // The pr_opened item carries the recorded title + url.
        let WaitItem::GithubEvent { title, url, .. } = &items[0].item else {
            unreachable!()
        };
        assert_eq!(title.as_deref(), Some("Add the widget"));
        assert_eq!(url.as_deref(), Some("https://github.com/o/r/pull/12"));
    }

    #[test]
    fn env_token_precedence_gh_token_beats_github_token_blanks_skipped() {
        // github-http-client-and-realtime M1: env beats shelling gh,
        // GH_TOKEN beats GITHUB_TOKEN, and blanks don't count.
        let get = |m: &[(&str, &str)]| {
            let m: Vec<(String, String)> = m
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            move |k: &str| m.iter().find(|(mk, _)| mk == k).map(|(_, v)| v.clone())
        };
        assert_eq!(
            env_token(get(&[("GH_TOKEN", "a"), ("GITHUB_TOKEN", "b")])),
            Some("a".into())
        );
        assert_eq!(env_token(get(&[("GITHUB_TOKEN", "b")])), Some("b".into()));
        assert_eq!(env_token(get(&[("GH_TOKEN", "  ")])), None, "blank skipped");
        assert_eq!(env_token(get(&[])), None);
    }

    #[tokio::test]
    async fn token_cell_reacquires_only_after_invalidation() {
        // A 401 invalidates; the next use re-acquires — pinned with a
        // scripted acquirer, no network.
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = AtomicUsize::new(0);
        let acquire = || {
            calls.fetch_add(1, Ordering::SeqCst);
            async { Ok(format!("t{}", calls.load(Ordering::SeqCst))) }
        };
        let cell = TokenCell::new();
        assert_eq!(cell.get_or_acquire(acquire).await.unwrap(), "t1");
        assert_eq!(cell.get_or_acquire(acquire).await.unwrap(), "t1", "cached");
        cell.invalidate().await;
        assert_eq!(
            cell.get_or_acquire(acquire).await.unwrap(),
            "t2",
            "re-acquired"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn webhook_classifier_maps_the_webhook_vocabulary() {
        use GithubEventKind::*;
        let all = &[
            PrOpened,
            PrUpdated,
            PrMerged,
            PrComment,
            IssueOpened,
            IssueClosed,
            IssueComment,
            BranchPush,
        ][..];
        let wh = |event: &str, payload: serde_json::Value| {
            classify_webhook("o/r", event, &payload, all, Some("me"), false, &[])
        };
        // Webhook shapes: top-level action, sender.login actor, review
        // action SUBMITTED (not the feed's created), push uses after.
        let cases = vec![
            (
                "pull_request",
                serde_json::json!({"action":"opened","sender":{"login":"x"},
                "pull_request":{"number":1,"title":"t","html_url":"u"}}),
                "pr_opened",
            ),
            (
                "pull_request",
                serde_json::json!({"action":"synchronize","sender":{"login":"x"},
                "pull_request":{"number":1}}),
                "pr_updated",
            ),
            (
                "pull_request",
                serde_json::json!({"action":"closed","sender":{"login":"x"},
                "pull_request":{"number":1,"merged":true}}),
                "pr_merged",
            ),
            (
                "pull_request_review",
                serde_json::json!({"action":"submitted","sender":{"login":"x"},
                "pull_request":{"number":1}}),
                "pr_comment",
            ),
            (
                "issue_comment",
                serde_json::json!({"action":"created","sender":{"login":"x"},
                "issue":{"number":2}}),
                "issue_comment",
            ),
        ];
        for (event, payload, want) in cases {
            let item = wh(event, payload)
                .unwrap_or_else(|| panic!("{event} → {want}"))
                .item;
            let WaitItem::GithubEvent { event: got, .. } = item else {
                unreachable!()
            };
            assert_eq!(got, want);
        }
        // push: branch gate + compare url from before/after; tag ignored;
        // own sender filtered.
        let push = serde_json::json!({"ref":"refs/heads/main","before":"aa","after":"bb",
            "commits":[{},{}],"sender":{"login":"x"}});
        let item = wh("push", push).expect("branch push").item;
        let WaitItem::GithubEvent { title, url, .. } = item else {
            unreachable!()
        };
        assert_eq!(title.as_deref(), Some("main +2"));
        assert_eq!(
            url.as_deref(),
            Some("https://github.com/o/r/compare/aa...bb")
        );
        assert!(
            wh(
                "push",
                serde_json::json!({"ref":"refs/tags/v1","before":"a","after":"b",
            "commits":[],"sender":{"login":"x"}})
            )
            .is_none(),
            "tag push ignored"
        );
        assert!(
            wh(
                "push",
                serde_json::json!({"ref":"refs/heads/main","before":"a","after":"b",
            "commits":[],"sender":{"login":"me"}})
            )
            .is_none(),
            "own push filtered"
        );
        // Non-creating webhook actions stay unmapped.
        assert!(
            wh(
                "pull_request_review",
                serde_json::json!({"action":"dismissed",
            "sender":{"login":"x"},"pull_request":{}})
            )
            .is_none()
        );
        assert!(
            wh(
                "pull_request",
                serde_json::json!({"action":"closed",
            "sender":{"login":"x"},"pull_request":{"merged":false}})
            )
            .is_none()
        );
    }

    #[test]
    fn action_keys_are_namespaced_and_never_manufactured() {
        // codex bf1e1ea: comment-family ids live in different resource
        // domains — the SAME numeric id across families must yield
        // DISTINCT keys…
        use GithubEventKind::*;
        let all = &[PrComment, IssueComment][..];
        let feed_key =
            |ty: &str, payload: serde_json::Value| {
                classify_event(
                "o/r",
                &serde_json::json!({"id":"1","type":ty,"actor":{"login":"x"},"payload":payload}),
                all, None, false, &[],
            )
            .expect("classifies")
            .key
            };
        let issue_c = feed_key(
            "IssueCommentEvent",
            serde_json::json!({"action":"created","comment":{"id":42},"issue":{"number":1}}),
        );
        let pr_issue_c = feed_key(
            "IssueCommentEvent",
            serde_json::json!({"action":"created","comment":{"id":42},
                "issue":{"number":1,"pull_request":{}}}),
        );
        let review_c = feed_key(
            "PullRequestReviewCommentEvent",
            serde_json::json!({"action":"created","comment":{"id":42},"pull_request":{"number":1}}),
        );
        let review = feed_key(
            "PullRequestReviewEvent",
            serde_json::json!({"action":"created","review":{"id":42},"pull_request":{"number":1}}),
        );
        let keys = [&issue_c, &pr_issue_c, &review_c, &review];
        for k in &keys {
            assert!(k.is_some(), "identity present → key present");
        }
        for (i, a) in keys.iter().enumerate() {
            for b in keys.iter().skip(i + 1) {
                assert_ne!(a, b, "same numeric id, different family → distinct keys");
            }
        }
        // …and a MISSING required identity yields None on both
        // transports (never a manufactured shared key).
        let missing_feed = feed_key(
            "PullRequestReviewEvent",
            serde_json::json!({"action":"created","pull_request":{"number":1}}),
        );
        assert_eq!(missing_feed, None, "no review.id → no key");
        let missing_hook = classify_webhook(
            "o/r",
            "pull_request",
            &serde_json::json!({"action":"synchronize","sender":{"login":"x"},
                "pull_request":{"number":12}}), // no head.sha
            &[PrUpdated],
            None,
            false,
            &[],
        )
        .expect("classifies")
        .key;
        assert_eq!(missing_hook, None, "no head sha → no key");
        // Push without its sha: item still wakes, key is None.
        let pushless = classify_webhook(
            "o/r",
            "push",
            &serde_json::json!({"ref":"refs/heads/main","before":"aa",
                "commits":[],"sender":{"login":"x"}}),
            &[BranchPush],
            None,
            false,
            &[],
        )
        .expect("classifies")
        .key;
        assert_eq!(pushless, None);
    }

    #[test]
    fn action_keys_are_identical_across_the_two_payload_forms() {
        // THE coordinator invariant's foundation (codex e89be23): one
        // underlying action, arriving as a feed event AND a webhook
        // delivery, derives ONE key — from the objects both forms
        // carry, never from feed ids or delivery GUIDs.
        use GithubEventKind::*;
        let all = &[
            PrOpened,
            PrUpdated,
            PrMerged,
            PrComment,
            IssueOpened,
            IssueClosed,
            IssueComment,
            BranchPush,
        ][..];
        let feed = |ty: &str, payload: serde_json::Value| {
            classify_event(
                "o/r",
                &serde_json::json!({"id":"40001","type":ty,"actor":{"login":"x"},"payload":payload}),
                all, None, false, &[],
            )
            .expect("feed classifies")
            .key
        };
        let hook = |name: &str, payload: serde_json::Value| {
            classify_webhook("o/r", name, &payload, all, None, false, &[])
                .expect("webhook classifies")
                .key
        };
        // pr_updated: head sha from pull_request.head.sha in both.
        let pr = serde_json::json!({"number":12,"head":{"sha":"beef"}});
        assert_eq!(
            feed(
                "PullRequestEvent",
                serde_json::json!({"action":"synchronize","pull_request":pr})
            ),
            hook(
                "pull_request",
                serde_json::json!({"action":"synchronize","sender":{"login":"x"},
                "pull_request":{"number":12,"head":{"sha":"beef"}}})
            ),
        );
        // review comment: comment.id in both.
        assert_eq!(
            feed(
                "PullRequestReviewCommentEvent",
                serde_json::json!({"action":"created",
                "comment":{"id":77},"pull_request":{"number":12}})
            ),
            hook(
                "pull_request_review_comment",
                serde_json::json!({"action":"created",
                "sender":{"login":"x"},"comment":{"id":77},"pull_request":{"number":12}})
            ),
        );
        // submitted review: review.id in both (feed action created,
        // webhook action submitted — same review).
        assert_eq!(
            feed(
                "PullRequestReviewEvent",
                serde_json::json!({"action":"created",
                "review":{"id":88},"pull_request":{"number":12}})
            ),
            hook(
                "pull_request_review",
                serde_json::json!({"action":"submitted",
                "sender":{"login":"x"},"review":{"id":88},"pull_request":{"number":12}})
            ),
        );
        // push: feed head == webhook after for one push.
        assert_eq!(
            feed(
                "PushEvent",
                serde_json::json!({"ref":"refs/heads/main","size":2,
                "before":"aa","head":"bb"})
            ),
            hook(
                "push",
                serde_json::json!({"ref":"refs/heads/main","before":"aa","after":"bb",
                "commits":[{},{}],"sender":{"login":"x"}})
            ),
        );
        // pr_opened / issues: number-keyed.
        assert_eq!(
            feed(
                "PullRequestEvent",
                serde_json::json!({"action":"opened","pull_request":{"number":5}})
            ),
            hook(
                "pull_request",
                serde_json::json!({"action":"opened","sender":{"login":"x"},
                "pull_request":{"number":5}})
            ),
        );
        assert_eq!(
            feed(
                "IssuesEvent",
                serde_json::json!({"action":"closed","issue":{"number":9}})
            ),
            hook(
                "issues",
                serde_json::json!({"action":"closed","sender":{"login":"x"},
                "issue":{"number":9}})
            ),
        );
    }

    #[tokio::test]
    async fn webhook_requests_map_to_raw_deliveries() {
        // The listener's extraction: X-GitHub-Event + JSON body →
        // RawDelivery; junk degrades to None (dropped, never a panic).
        use http_body_util::Full;
        use hyper::body::Bytes;
        let req = hyper::Request::builder()
            .header("X-GitHub-Event", "push")
            .body(Full::new(Bytes::from(r#"{"ref":"refs/heads/main"}"#)))
            .unwrap();
        let (name, payload) = webhook_from_request(req).await.expect("maps");
        assert_eq!(name, "push");
        assert_eq!(payload["ref"], "refs/heads/main");
        // Missing event header → None.
        let req = hyper::Request::builder()
            .body(Full::new(Bytes::from("{}")))
            .unwrap();
        assert!(webhook_from_request(req).await.is_none());
        // Non-JSON body → None.
        let req = hyper::Request::builder()
            .header("X-GitHub-Event", "push")
            .body(Full::new(Bytes::from("not json")))
            .unwrap();
        assert!(webhook_from_request(req).await.is_none());
    }

    #[tokio::test]
    async fn relay_listener_binds_loopback_only() {
        // "Rejects non-loopback binds by construction": the bind is
        // literally 127.0.0.1.
        let l = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        assert!(l.local_addr().unwrap().ip().is_loopback());
    }

    #[test]
    fn webhook_event_names_cover_the_subscribed_kinds() {
        use GithubEventKind::*;
        let names = webhook_event_names(&[PrOpened, PrComment, BranchPush]);
        for expect in [
            "pull_request",
            "pull_request_review",
            "pull_request_review_comment",
            "issue_comment",
            "push",
        ] {
            assert!(names.contains(&expect), "{expect} missing: {names:?}");
        }
        assert_eq!(
            webhook_event_names(&[IssueOpened, IssueClosed]),
            vec!["issues"],
            "no over-subscription"
        );
    }

    #[test]
    fn forwarder_argv_is_pinned() {
        assert_eq!(
            forwarder_argv("o/r", "push,issues", 4242),
            vec![
                "webhook",
                "forward",
                "--repo=o/r",
                "--events=push,issues",
                "--url=http://127.0.0.1:4242",
            ]
        );
    }

    #[tokio::test]
    async fn oversized_webhook_bodies_are_rejected() {
        // codex 4d77b83: a count-bounded inbox doesn't bound memory if
        // one request is huge — the body collect is byte-limited.
        use http_body_util::Full;
        use hyper::body::Bytes;
        let big = vec![b'x'; MAX_WEBHOOK_BODY + 1];
        let req = hyper::Request::builder()
            .header("X-GitHub-Event", "push")
            .body(Full::new(Bytes::from(big)))
            .unwrap();
        assert!(webhook_from_request(req).await.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn relay_start_failure_and_exit_route_one_reason_through_the_inbox() {
        // The injected supervisor seam (codex 4d77b83): a scripted
        // forwarder terminates with its reason; the inbox closes
        // carrying it (the SINGLE diagnostic's payload), the poll
        // catches the action the relay missed, and the guard's drop
        // aborts the relay tasks.
        struct ScriptForwarder(&'static str);
        impl Forwarder for ScriptForwarder {
            async fn run(&self, _repo: String, _events: String, _port: u16) -> String {
                self.0.to_string()
            }
        }
        let s = src(&[GithubEventKind::IssueOpened], false);
        let (inbox, guard) = start_relay(&s, ScriptForwarder("extension missing"))
            .await
            .expect("listener binds");
        // The forwarder terminated immediately: the inbox closes with
        // the bounded reason.
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(5), inbox.recv())
                .await
                .expect("closes"),
            None
        );
        assert_eq!(inbox.reason(), "extension missing");
        // The poll still catches the action the dead relay never
        // delivered (baseline, then the delta poll emits it).
        let f = ScriptFetcher::new(vec![
            Box::new(|_p, _e| Ok(ok(vec![], None, None))),
            Box::new(|_p, _e| Ok(ok(vec![issue("7", 7)], None, None))),
        ]);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let loop_inbox = inbox.clone();
        let handle = tokio::spawn(async move {
            poll_loop(
                &src(&[GithubEventKind::IssueOpened], false),
                &FixedLogin("me"),
                &f,
                &tx,
                Some(loop_inbox),
                None,
            )
            .await;
        });
        let item = tokio::time::timeout(std::time::Duration::from_secs(300), rx.recv())
            .await
            .expect("poll-only recovery")
            .expect("open");
        let WaitItem::GithubEvent { number, .. } = item else {
            unreachable!()
        };
        assert_eq!(number, Some(7));
        handle.abort();
        let _ = handle.await;
        drop(guard);
    }

    #[tokio::test]
    async fn relay_guard_drop_aborts_both_owned_tasks() {
        // codex 24c3355: an observable cancellation probe — each task
        // holds a set-on-drop guard; RelayGuard's drop must abort them
        // so the flags flip.
        use std::sync::atomic::{AtomicBool, Ordering};
        struct SetOnDrop(std::sync::Arc<AtomicBool>);
        impl Drop for SetOnDrop {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let flags = [
            std::sync::Arc::new(AtomicBool::new(false)),
            std::sync::Arc::new(AtomicBool::new(false)),
        ];
        let tasks: Vec<_> = flags
            .iter()
            .map(|f| {
                let probe = SetOnDrop(f.clone());
                tokio::spawn(async move {
                    let _probe = probe;
                    std::future::pending::<()>().await;
                })
            })
            .collect();
        tokio::task::yield_now().await;
        drop(RelayGuard(tasks));
        // Aborted tasks unwind asynchronously; give the runtime a few
        // turns.
        for _ in 0..50 {
            if flags.iter().all(|f| f.load(Ordering::SeqCst)) {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("RelayGuard drop did not cancel its tasks");
    }

    #[tokio::test]
    async fn forwarder_reason_survives_a_pipe_filling_child() {
        // codex 24c3355: a child that writes beyond the OS pipe
        // capacity before exiting must still reach its terminal
        // reason — the concurrent tail read keeps the pipe drained.
        // 256 KiB of stderr ≫ any default pipe buffer.
        let mut cmd = tokio::process::Command::new("sh");
        cmd.args([
            "-c",
            "i=0; while [ $i -lt 4096 ]; do printf \
             'xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\\n' >&2; \
             i=$((i+1)); done; echo the-final-line >&2; exit 3",
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
        let reason = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            run_forwarder_process(cmd),
        )
        .await
        .expect("no deadlock on a full pipe");
        assert!(
            reason.contains("the-final-line"),
            "the ACTUAL last stderr line survives the bounded tail: {reason}"
        );
        assert!(reason.contains("exited"), "{reason}");
    }

    // ── the coordinator: one emission owner, baseline horizon ──

    fn classified(n: u64, key: Option<&str>) -> Classified {
        Classified {
            item: WaitItem::GithubEvent {
                repo: "o/r".into(),
                event: "issue_opened".into(),
                detail: None,
                number: Some(n),
                title: None,
                actor: None,
                url: None,
            },
            key: key.map(str::to_string),
            feed_id: None,
            event_at: None,
        }
    }

    #[tokio::test]
    async fn coordinator_gates_emission_by_action_key() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut c = Coordinator::new(tx);
        assert!(c.baseline(vec![classified(1, Some("issue_opened#1"))]));
        // Baseline emitted nothing.
        assert!(rx.try_recv().is_err());
        // webhook-then-poll of the SAME action → exactly one emission.
        assert!(c.deliver(classified(2, Some("issue_opened#2"))));
        assert!(c.offer(classified(2, Some("issue_opened#2"))));
        assert!(rx.try_recv().is_ok(), "the first arrival emitted");
        assert!(rx.try_recv().is_err(), "the second path's copy dropped");
        // A poll-only action (the relay missed it) still emits.
        assert!(c.offer(classified(3, Some("issue_opened#3"))));
        assert!(rx.try_recv().is_ok());
        // A pre-baseline action delivered later by webhook → no wake.
        assert!(c.deliver(classified(1, Some("issue_opened#1"))));
        assert!(rx.try_recv().is_err(), "baseline-seeded key drops");
        // KEYLESS wakes are never deduped.
        assert!(c.offer(classified(9, None)));
        assert!(c.offer(classified(9, None)));
        assert!(rx.try_recv().is_ok());
        assert!(rx.try_recv().is_ok(), "keyless = emit-always");
    }

    #[tokio::test]
    async fn coordinator_buffers_relay_deliveries_until_the_baseline() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut c = Coordinator::new(tx);
        // Deliveries BEFORE the baseline buffer silently…
        assert!(c.deliver(classified(1, Some("issue_opened#1"))));
        assert!(c.deliver(classified(5, Some("issue_opened#5"))));
        assert!(rx.try_recv().is_err(), "nothing emits pre-baseline");
        // …then the baseline seeds (action 1 was visible at arm time)
        // and the backlog drains through the gate: 1 drops, 5 emits.
        assert!(c.baseline(vec![classified(1, Some("issue_opened#1"))]));
        let emitted = rx.try_recv().expect("the genuinely-new action");
        let WaitItem::GithubEvent { number, .. } = emitted else {
            unreachable!()
        };
        assert_eq!(number, Some(5));
        assert!(
            rx.try_recv().is_err(),
            "the pre-baseline action never wakes"
        );
    }

    #[tokio::test]
    async fn coordinator_buffer_is_bounded_drop_oldest() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut c = Coordinator::new(tx);
        for i in 0..(RELAY_BUFFER_CAP as u64 + 3) {
            assert!(c.deliver(classified(i, Some(&format!("issue_opened#{i}")))));
        }
        assert!(c.baseline(vec![]));
        let mut got = Vec::new();
        while let Ok(item) = rx.try_recv() {
            let WaitItem::GithubEvent { number, .. } = item else {
                unreachable!()
            };
            got.push(number.unwrap());
        }
        assert_eq!(got.len(), RELAY_BUFFER_CAP, "bounded backlog");
        assert_eq!(*got.first().unwrap(), 3, "oldest dropped first");
    }

    // ── session auth: sharing + uniform 401 recovery ──────────

    struct ScriptHttp {
        /// Scripted statuses per request, in order; exhausted → 200.
        statuses: std::sync::Mutex<std::collections::VecDeque<u16>>,
        calls: std::sync::atomic::AtomicUsize,
    }
    impl ScriptHttp {
        fn new(statuses: &[u16]) -> Self {
            Self {
                statuses: std::sync::Mutex::new(statuses.iter().copied().collect()),
                calls: std::sync::atomic::AtomicUsize::new(0),
            }
        }
    }
    impl HttpSend for ScriptHttp {
        async fn send(
            &self,
            url: &str,
            _bearer: &str,
            _etag: Option<&str>,
        ) -> anyhow::Result<MiniResponse> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let status = self.statuses.lock().unwrap().pop_front().unwrap_or(200);
            let body = if url.ends_with("/user") {
                r#"{"login":"me"}"#.to_string()
            } else {
                "[]".to_string()
            };
            Ok(MiniResponse {
                status,
                headers: Default::default(),
                body,
            })
        }
    }

    static ACQUIRES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    fn counting_acquire()
    -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<String>> + Send>> {
        ACQUIRES.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Box::pin(async { Ok("tok".to_string()) })
    }

    #[tokio::test]
    async fn one_session_serves_many_sources_with_one_acquisition_and_uniform_401() {
        // codex f9807c3, all four demands in one scripted session:
        ACQUIRES.store(0, std::sync::atomic::Ordering::SeqCst);
        // (1) two fetchers share the session → ONE acquisition.
        let session = std::sync::Arc::new(GithubSession::for_tests(
            ScriptHttp::new(&[200, 200]),
            counting_acquire,
        ));
        let a = GhFetcher {
            repo: "o/a".into(),
            session: session.clone(),
        };
        let b = GhFetcher {
            repo: "o/b".into(),
            session: session.clone(),
        };
        a.fetch(1, None).await.unwrap();
        b.fetch(1, None).await.unwrap();
        assert_eq!(
            ACQUIRES.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "one acquisition for both sources"
        );

        // (2) /user recovers after one 401 (invalidate → re-acquire →
        // retry once).
        ACQUIRES.store(0, std::sync::atomic::Ordering::SeqCst);
        let session = std::sync::Arc::new(GithubSession::for_tests(
            ScriptHttp::new(&[401, 200]),
            counting_acquire,
        ));
        assert_eq!(fetch_authenticated_login(&session).await.unwrap(), "me");
        assert_eq!(
            ACQUIRES.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "initial + one re-acquire"
        );

        // (3) /events recovers the same way — one shared 401 story.
        let session = std::sync::Arc::new(GithubSession::for_tests(
            ScriptHttp::new(&[401, 200]),
            counting_acquire,
        ));
        let f = GhFetcher {
            repo: "o/r".into(),
            session,
        };
        assert!(f.fetch(1, None).await.is_ok());

        // (4) a second 401 fails WITHOUT retrying forever.
        let http = ScriptHttp::new(&[401, 401]);
        let session = std::sync::Arc::new(GithubSession::for_tests(http, counting_acquire));
        let f = GhFetcher {
            repo: "o/r".into(),
            session: session.clone(),
        };
        let err = f.fetch(1, None).await.unwrap_err();
        assert!(err.to_string().contains("401"), "{err}");
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
            .filter_map(|c| match &c.item {
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
                None,
                None,
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
                None,
                None,
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
                None,
                None,
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
                None,
                None,
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

    #[tokio::test(start_paused = true)]
    async fn relay_deliveries_emit_immediately_and_dedup_against_the_poll() {
        // The full M2 lifecycle through the REAL poll_loop, no gh and
        // no sockets: baseline poll seeds the horizon; a relay
        // delivery emits IMMEDIATELY between ticks; the next poll's
        // copy of the SAME action (same key) is suppressed by the
        // coordinator; a poll-only action still emits; and the relay
        // closing falls back to polling alone.
        let s = src(&[GithubEventKind::IssueOpened], false);
        let f = ScriptFetcher::new(vec![
            // Tick 1: the baseline — issue #1 exists at arm time.
            Box::new(|_p, _e| Ok(ok(vec![issue("1", 1)], Some("\"e1\""), None))),
            Box::new(|_p, _e| Ok(ok(vec![], None, None))),
            // Tick 2: the feed catches up — issue #2 (already emitted
            // via the relay: SAME action key) and issue #3 (poll-only).
            Box::new(|_p, _e| {
                Ok(ok(
                    vec![
                        serde_json::json!({"id":"102","type":"IssuesEvent","actor":{"login":"them"},
                            "payload":{"action":"opened","issue":{"number":2}}}),
                        serde_json::json!({"id":"103","type":"IssuesEvent","actor":{"login":"them"},
                            "payload":{"action":"opened","issue":{"number":3}}}),
                        issue("1", 1),
                    ],
                    None,
                    None,
                ))
            }),
        ]);
        let inbox = std::sync::Arc::new(RelayInbox::new());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let loop_inbox = inbox.clone();
        let handle = tokio::spawn(async move {
            poll_loop(&s, &FixedLogin("me"), &f, &tx, Some(loop_inbox), None).await;
        });
        // A webhook delivery for issue #2 lands between ticks — it must
        // emit without waiting for the next poll.
        inbox.push((
            "issues".to_string(),
            serde_json::json!({"action":"opened","sender":{"login":"them"},
                "issue":{"number":2}}),
        ));
        let first = tokio::time::timeout(std::time::Duration::from_secs(300), rx.recv())
            .await
            .expect("the relay delivery emits")
            .expect("open");
        let WaitItem::GithubEvent { number, .. } = first else {
            unreachable!()
        };
        assert_eq!(number, Some(2), "relay latency path");
        // The next poll carries #2 (suppressed — same key) and #3
        // (poll-only → emits). Exactly ONE more emission, #3.
        let second = tokio::time::timeout(std::time::Duration::from_secs(300), rx.recv())
            .await
            .expect("the poll-only action emits")
            .expect("open");
        let WaitItem::GithubEvent { number, .. } = second else {
            unreachable!()
        };
        assert_eq!(number, Some(3), "the poll's copy of #2 was suppressed");
        // Relay closes → the loop continues on polling alone (script
        // exhausted → 304s forever; the loop stays alive).
        inbox.close();
        tokio::task::yield_now().await;
        assert!(rx.try_recv().is_err(), "no duplicate of #2 ever surfaced");
        handle.abort();
        let _ = handle.await;
    }

    #[tokio::test(start_paused = true)]
    async fn pre_login_relay_intake_is_bounded_and_filtered_after_resolution() {
        // codex d7f4a8f: while /user is down the relay channel must not
        // grow unboundedly — intake drains into the bounded drop-oldest
        // backlog, and classification waits for the login so a
        // SELF-AUTHORED delivery buffered pre-login is filtered after
        // resolution (fail-closed own-actor, preserved end to end).
        let login = ScriptLogin(std::sync::Mutex::new(
            vec![
                Err(anyhow::anyhow!("gh down")),
                Err(anyhow::anyhow!("still down")),
                Ok("me".to_string()),
            ]
            .into(),
        ));
        // Fetcher: the (post-login) baseline is empty; then 304s.
        let f = ScriptFetcher::new(vec![Box::new(|_p, _e| Ok(ok(vec![], None, None)))]);
        let inbox = std::sync::Arc::new(RelayInbox::new());
        // MORE than the cap of foreign deliveries + one self-authored,
        // all pushed while the login is still failing — the bound
        // applies at INGRESS (codex 840725c).
        for i in 0..(RELAY_BUFFER_CAP as u64 + 3) {
            inbox.push((
                "issues".to_string(),
                serde_json::json!({"action":"opened","sender":{"login":"them"},
                    "issue":{"number":i}}),
            ));
        }
        inbox.push((
            "issues".to_string(),
            serde_json::json!({"action":"opened","sender":{"login":"me"},
                "issue":{"number":9999}}),
        ));
        assert_eq!(inbox.len(), RELAY_BUFFER_CAP, "bounded at the inbox itself");
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let loop_inbox = inbox.clone();
        let handle = tokio::spawn(async move {
            poll_loop(
                &src(&[GithubEventKind::IssueOpened], false),
                &login,
                &f,
                &tx,
                Some(loop_inbox),
                None,
            )
            .await;
        });
        // Expected: 259 foreign + 1 own sent; the bound keeps the LAST
        // 256 (foreign 4..=258 plus the own one); the own delivery is
        // filtered at classification ⇒ 255 emissions, starting at 4.
        let expect = RELAY_BUFFER_CAP - 1;
        let got: Vec<u64> = tokio::time::timeout(std::time::Duration::from_secs(600), async {
            let mut got = Vec::new();
            while got.len() < expect {
                let Some(item) = rx.recv().await else { break };
                let WaitItem::GithubEvent { number, .. } = item else {
                    unreachable!()
                };
                got.push(number.unwrap());
            }
            got
        })
        .await
        .expect("the bounded backlog drains after login+baseline");
        assert_eq!(got.len(), expect, "drop-oldest bound applied");
        assert!(!got.contains(&9999), "own delivery filtered post-login");
        assert_eq!(*got.first().unwrap(), 4, "oldest dropped first");
        // Nothing further: the own delivery never emits.
        tokio::task::yield_now().await;
        assert!(rx.try_recv().is_err());
        handle.abort();
        let _ = handle.await;
    }

    #[tokio::test(start_paused = true)]
    async fn inbox_bounds_ingress_while_login_resolution_hangs() {
        // codex 840725c: the bound must hold even while the consumer is
        // stuck INSIDE an await — a login resolution that never
        // returns. Producers exceed the cap; the inbox itself evicts.
        struct PendingLogin;
        impl LoginResolver for PendingLogin {
            async fn resolve(&self) -> anyhow::Result<String> {
                std::future::pending::<()>().await;
                unreachable!()
            }
        }
        let f = ScriptFetcher::new(vec![]);
        let inbox = std::sync::Arc::new(RelayInbox::new());
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let loop_inbox = inbox.clone();
        let handle = tokio::spawn(async move {
            poll_loop(
                &src(&[GithubEventKind::IssueOpened], false),
                &PendingLogin,
                &f,
                &tx,
                Some(loop_inbox),
                None,
            )
            .await;
        });
        tokio::task::yield_now().await; // the loop is now inside resolve()
        for i in 0..(RELAY_BUFFER_CAP as u64 * 2) {
            inbox.push((
                "issues".to_string(),
                serde_json::json!({"action":"opened","sender":{"login":"them"},
                    "issue":{"number":i}}),
            ));
        }
        assert_eq!(
            inbox.len(),
            RELAY_BUFFER_CAP,
            "eviction at the inbox boundary, consumer hung or not"
        );
        handle.abort();
        let _ = handle.await;
    }

    #[tokio::test(start_paused = true)]
    async fn relay_closing_while_login_gated_falls_back_without_ending_the_loop() {
        // The relay dies while the source is still login-gated: one
        // fallback line, and the poll loop keeps retrying login and
        // then polls normally.
        let login = ScriptLogin(std::sync::Mutex::new(
            vec![Err(anyhow::anyhow!("gh down")), Ok("me".to_string())].into(),
        ));
        let f = ScriptFetcher::new(vec![
            // Baseline (post-login), then a delta with a new action —
            // proof the loop survived the relay's death.
            Box::new(|_p, _e| Ok(ok(vec![], None, None))),
            Box::new(|_p, _e| Ok(ok(vec![issue("7", 7)], None, None))),
        ]);
        let inbox = std::sync::Arc::new(RelayInbox::new());
        inbox.close(); // dead before login ever resolves
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let loop_inbox = inbox.clone();
        let handle = tokio::spawn(async move {
            poll_loop(
                &src(&[GithubEventKind::IssueOpened], false),
                &login,
                &f,
                &tx,
                Some(loop_inbox),
                None,
            )
            .await;
        });
        let item = tokio::time::timeout(std::time::Duration::from_secs(300), rx.recv())
            .await
            .expect("the loop out-lives the relay and still polls")
            .expect("open");
        let WaitItem::GithubEvent { number, .. } = item else {
            unreachable!()
        };
        assert_eq!(number, Some(7));
        handle.abort();
        let _ = handle.await;
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
                None,
                None,
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

    // ── event_at capture (log-timeline-github-events) ──

    #[test]
    fn iso8601_epoch_parses_github_shapes() {
        assert_eq!(parse_iso8601_epoch("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_iso8601_epoch("2026-07-29T01:02:03Z"),
            Some(1_785_286_923)
        );
        // Fractional seconds are valid RFC3339 (codex 6219148).
        assert_eq!(
            parse_iso8601_epoch("2026-07-29T01:02:03.517Z"),
            Some(1_785_286_923)
        );
        // Offset form (push head_commit.timestamp): +10:00 is 10h
        // BEHIND the same wall-clock in UTC.
        assert_eq!(
            parse_iso8601_epoch("2026-07-29T11:02:03+10:00"),
            Some(1_785_286_923)
        );
        assert_eq!(
            parse_iso8601_epoch("2026-07-28T15:02:03-10:00"),
            Some(1_785_286_923)
        );
        // Malformed external data must be None — never a
        // plausible-but-wrong epoch that reorders the timeline
        // (codex 6219148).
        for bad in [
            "",
            "not a time",
            "2026-07-29",
            "2026-07-29T01:02:03",
            "2026-13-01T00:00:00Z",
            "2026-02-31T00:00:00Z",
            "2026-07-29T01:02:03+99:99",
            "1969-12-31T23:59:59Z",
        ] {
            assert_eq!(parse_iso8601_epoch(bad), None, "{bad}");
        }
    }

    #[test]
    fn classifiers_capture_event_time_best_effort() {
        // Feed: top-level created_at covers every kind uniformly.
        let ev = serde_json::json!({"id":"1","type":"IssuesEvent",
            "created_at":"2026-07-29T01:02:03Z",
            "actor":{"login":"them"},
            "payload":{"action":"opened","issue":{"number":1}}});
        let c =
            classify_event("o/r", &ev, &[GithubEventKind::IssueOpened], None, true, &[]).unwrap();
        assert_eq!(c.event_at, Some(1_785_286_923));
        // Feed without created_at → None (fallback is the caller's).
        let ev = serde_json::json!({"id":"1","type":"IssuesEvent",
            "actor":{"login":"them"},
            "payload":{"action":"opened","issue":{"number":1}}});
        let c =
            classify_event("o/r", &ev, &[GithubEventKind::IssueOpened], None, true, &[]).unwrap();
        assert_eq!(c.event_at, None);
        // Webhook comment.created_at.
        let c = classify_webhook(
            "o/r",
            "issue_comment",
            &serde_json::json!({"action":"created","sender":{"login":"them"},
                "issue":{"number":2},
                "comment":{"id":9,"created_at":"2026-07-29T01:02:03Z"}}),
            &[GithubEventKind::IssueComment],
            None,
            true,
            &[],
        )
        .unwrap();
        assert_eq!(c.event_at, Some(1_785_286_923));
        // Webhook review.submitted_at.
        let c = classify_webhook(
            "o/r",
            "pull_request_review",
            &serde_json::json!({"action":"submitted","sender":{"login":"them"},
                "pull_request":{"number":3},
                "review":{"id":8,"submitted_at":"2026-07-29T01:02:03Z"}}),
            &[GithubEventKind::PrComment],
            None,
            true,
            &[],
        )
        .unwrap();
        assert_eq!(c.event_at, Some(1_785_286_923));
        // Webhook push head_commit.timestamp (offset form).
        let c = classify_webhook(
            "o/r",
            "push",
            &serde_json::json!({"sender":{"login":"them"},
                "ref":"refs/heads/main","before":"a","after":"b",
                "commits":[{}],
                "head_commit":{"timestamp":"2026-07-29T11:02:03+10:00"}}),
            &[GithubEventKind::BranchPush],
            None,
            true,
            &[],
        )
        .unwrap();
        assert_eq!(c.event_at, Some(1_785_286_923));
    }

    // ── the WAL wiring: offline catch-up end to end ──

    fn wal_tempdir() -> std::path::PathBuf {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "clank-gh-wal-wiring-{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn open_log(dir: &std::path::Path) -> crate::cli::github_event_log::EventLog {
        crate::cli::github_event_log::EventLog::open(dir, "t").unwrap()
    }

    /// Cold arm: the baseline writes observation records; a SECOND arm
    /// over the same dir is warm — the event that fired between the
    /// two runs EMITS on the second arm's first fetch instead of being
    /// swallowed by a fresh baseline. The core offline-catch-up
    /// acceptance.
    #[tokio::test(start_paused = true)]
    async fn event_between_runs_is_presented_by_the_second_run() {
        let dir = wal_tempdir();
        // Run 1: baseline sees issue #1, then the wait dies (abort).
        let f = ScriptFetcher::new(vec![Box::new(|_p, _e| {
            Ok(ok(vec![issue("1", 1)], Some("\"e1\""), None))
        })]);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let s1 = src(&[GithubEventKind::IssueOpened], true);
        let d = dir.clone();
        let run1 = tokio::spawn(async move {
            poll_loop(&s1, &FixedLogin("me"), &f, &tx, None, Some(open_log(&d))).await;
        });
        tokio::task::yield_now().await;
        assert!(
            rx.try_recv().is_err(),
            "baseline presents nothing (cold arm)"
        );
        run1.abort();
        let _ = run1.await;

        // Between runs: issue #2 fires. Run 2 arms warm and reconciles.
        let f = ScriptFetcher::new(vec![Box::new(|_p, _e| {
            Ok(ok(vec![issue("2", 2), issue("1", 1)], None, None))
        })]);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let s2 = src(&[GithubEventKind::IssueOpened], true);
        let d = dir.clone();
        let run2 = tokio::spawn(async move {
            poll_loop(&s2, &FixedLogin("me"), &f, &tx, None, Some(open_log(&d))).await;
        });
        let woke = tokio::time::timeout(std::time::Duration::from_secs(300), rx.recv())
            .await
            .expect("the missed event must wake the second run")
            .expect("open");
        let WaitItem::GithubEvent { number, .. } = &woke else {
            unreachable!()
        };
        assert_eq!(*number, Some(2), "only the NEW event emits, not history");
        // WAL-first: the emitted event is durably unhandled.
        let loaded = open_log(&dir).load();
        assert_eq!(loaded.unhandled.len(), 1);
        assert_eq!(loaded.unhandled[0].item, woke, "byte-faithful record");
        run2.abort();
        let _ = run2.await;
    }

    /// A killed wait between emission and reaction: the unhandled
    /// entry re-presents IMMEDIATELY at the next arm, before any
    /// fetch — and an ack stops the re-presenting.
    #[tokio::test(start_paused = true)]
    async fn unhandled_backlog_represents_at_arm_until_acked() {
        let dir = wal_tempdir();
        {
            let mut log = open_log(&dir);
            log.append_observation(Some("50"), None, true).unwrap();
            log.append_inbox(
                crate::cli::github_event_log::Transport::Poll,
                Some("51"),
                Some("issue#9"),
                None,
                &classified(9, Some("issue#9")).item,
            )
            .unwrap();
        }
        // The fetcher would block forever — the backlog must arrive
        // WITHOUT any fetch completing.
        let f = ScriptFetcher::new(vec![]);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let s = src(&[GithubEventKind::IssueOpened], true);
        let d = dir.clone();
        let run = tokio::spawn(async move {
            poll_loop(&s, &FixedLogin("me"), &f, &tx, None, Some(open_log(&d))).await;
        });
        let woke = tokio::time::timeout(std::time::Duration::from_secs(300), rx.recv())
            .await
            .expect("backlog wakes the arm")
            .expect("open");
        assert_eq!(woke, classified(9, Some("issue#9")).item);
        run.abort();
        let _ = run.await;

        // Ack it: the next arm has nothing to re-present.
        let mut log = open_log(&dir);
        let seq = log.load().unhandled[0].seq;
        log.append_ack(seq).unwrap();
        assert!(open_log(&dir).load().unhandled.is_empty());
    }

    /// codex 7c9e7fe: a quarantined WAL with a SURVIVING state.json
    /// must not condition the first fetch — a stale ETag could 304
    /// forever and the baseline (which unblocks buffered relay
    /// deliveries) would never establish. Cold start = unconditional
    /// fetch, real baseline, relay drains.
    #[tokio::test(start_paused = true)]
    async fn cold_start_ignores_surviving_etag_and_establishes_baseline() {
        let dir = wal_tempdir();
        {
            // A valid cache from a previous life…
            let log = open_log(&dir);
            log.save_state(Some("\"stale\""), Some(1)).unwrap();
            // …and a WAL that quarantines (interior corruption).
            std::fs::write(
                dir.join("t.jsonl"),
                "not json\n{\"t\":\"ack\",\"seq\":1,\"at\":1}\n",
            )
            .unwrap();
        }
        // Conditioned request → 304 (the trap); unconditional → Ok.
        // One closure per PAGE fetch: tick 1 = pages 1+2 (boundary),
        // tick 2 = new event page + fully-seen boundary page.
        let f = ScriptFetcher::new(vec![
            Box::new(|_p, etag| {
                if etag.is_some() {
                    Ok(GhResponse::NotModified {
                        poll_interval: None,
                    })
                } else {
                    Ok(ok(vec![issue("1", 1)], Some("\"fresh\""), None))
                }
            }),
            Box::new(|_p, _e| Ok(ok(vec![], None, None))),
            // Tick 2 after the baseline: a genuinely new event emits.
            Box::new(|_p, _e| Ok(ok(vec![issue("2", 2)], None, None))),
            Box::new(|_p, _e| Ok(ok(vec![issue("1", 1)], None, None))),
        ]);
        let inbox = std::sync::Arc::new(RelayInbox::new());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let s = src(&[GithubEventKind::IssueOpened], true);
        let d = dir.clone();
        let loop_inbox = inbox.clone();
        // A pre-baseline relay delivery: only a REAL baseline drains it.
        inbox.push((
            "issues".to_string(),
            serde_json::json!({"action":"opened","sender":{"login":"them"},
                "issue":{"number":5}}),
        ));
        let run = tokio::spawn(async move {
            poll_loop(
                &s,
                &FixedLogin("me"),
                &f,
                &tx,
                Some(loop_inbox),
                Some(open_log(&d)),
            )
            .await;
        });
        let first = tokio::time::timeout(std::time::Duration::from_secs(300), rx.recv())
            .await
            .expect("the baseline must establish and drain the buffered relay delivery")
            .expect("open");
        let WaitItem::GithubEvent { number, .. } = &first else {
            unreachable!()
        };
        assert_eq!(*number, Some(5), "the buffered relay delivery drained");
        let second = tokio::time::timeout(std::time::Duration::from_secs(300), rx.recv())
            .await
            .expect("the post-baseline delta emits")
            .expect("open");
        let WaitItem::GithubEvent { number, .. } = &second else {
            unreachable!()
        };
        assert_eq!(*number, Some(2));
        run.abort();
        let _ = run.await;
    }

    /// codex 7c9e7fe: pagination shift repeating a PRESENTABLE event
    /// across pages within one tick must produce ONE emission and ONE
    /// primary WAL record.
    #[tokio::test(start_paused = true)]
    async fn repeated_presentable_id_across_pages_emits_and_records_once() {
        let dir = wal_tempdir();
        // One closure per PAGE fetch: baseline pages, then a delta
        // tick whose pagination SHIFTS issue #2 (id 20) onto page 2 as
        // well — the same presentable event observed twice in one tick.
        let f = ScriptFetcher::new(vec![
            Box::new(|_p, _e| Ok(ok(vec![issue("1", 1)], Some("\"e\""), None))),
            Box::new(|_p, _e| Ok(ok(vec![], None, None))),
            Box::new(|_p, _e| Ok(ok(vec![issue("20", 2)], None, None))),
            Box::new(|_p, _e| Ok(ok(vec![issue("20", 2), issue("1", 1)], None, None))),
            Box::new(|_p, _e| Ok(ok(vec![], None, None))),
        ]);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let s = src(&[GithubEventKind::IssueOpened], true);
        let d = dir.clone();
        let run = tokio::spawn(async move {
            poll_loop(&s, &FixedLogin("me"), &f, &tx, None, Some(open_log(&d))).await;
        });
        let woke = tokio::time::timeout(std::time::Duration::from_secs(300), rx.recv())
            .await
            .expect("the new event emits once")
            .expect("open");
        let WaitItem::GithubEvent { number, .. } = &woke else {
            unreachable!()
        };
        assert_eq!(*number, Some(2));
        // No second emission from the page-2 copy.
        tokio::time::sleep(std::time::Duration::from_secs(120)).await;
        assert!(rx.try_recv().is_err(), "page-repeated copy double-emitted");
        run.abort();
        let _ = run.await;
        // Exactly one primary record for feed id 20: it's unhandled
        // (inbox) and appears once in the cursor.
        let loaded = open_log(&dir).load();
        assert_eq!(loaded.unhandled.len(), 1);
        assert_eq!(
            loaded.feed_ids.iter().filter(|id| *id == "20").count(),
            1,
            "one primary record, one cursor entry"
        );
    }

    /// codex 82a221a's restart scenario: a relay-emitted action whose
    /// event the poll NEVER observed must not double-emit after a
    /// restart — the action-key horizon is rebuilt from the WAL.
    #[tokio::test(start_paused = true)]
    async fn relay_emitted_action_stays_suppressed_across_restart() {
        let dir = wal_tempdir();
        // Run 1: empty baseline, then a relay delivery emits issue #2.
        let f = ScriptFetcher::new(vec![Box::new(|_p, _e| Ok(ok(vec![], Some("\"e\""), None)))]);
        let inbox = std::sync::Arc::new(RelayInbox::new());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let s1 = src(&[GithubEventKind::IssueOpened], true);
        let d = dir.clone();
        let loop_inbox = inbox.clone();
        let run1 = tokio::spawn(async move {
            poll_loop(
                &s1,
                &FixedLogin("me"),
                &f,
                &tx,
                Some(loop_inbox),
                Some(open_log(&d)),
            )
            .await;
        });
        inbox.push((
            "issues".to_string(),
            serde_json::json!({"action":"opened","sender":{"login":"them"},
                "issue":{"number":2}}),
        ));
        let first = tokio::time::timeout(std::time::Duration::from_secs(300), rx.recv())
            .await
            .expect("relay emits")
            .expect("open");
        let WaitItem::GithubEvent { number, .. } = &first else {
            unreachable!()
        };
        assert_eq!(*number, Some(2));
        run1.abort();
        let _ = run1.await;
        // Ack it (the agent reacted) so run 2's backlog is empty and
        // any second emission could only be a dedup failure.
        {
            let mut log = open_log(&dir);
            let seq = log.load().unhandled[0].seq;
            log.append_ack(seq).unwrap();
        }

        // Run 2: the poll NOW observes the same action from the feed
        // (new feed id, same action key). It must be suppressed.
        let f = ScriptFetcher::new(vec![Box::new(|_p, _e| {
            Ok(ok(vec![issue("77", 2)], None, None))
        })]);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let s2 = src(&[GithubEventKind::IssueOpened], true);
        let d = dir.clone();
        let run2 = tokio::spawn(async move {
            poll_loop(&s2, &FixedLogin("me"), &f, &tx, None, Some(open_log(&d))).await;
        });
        // Give the loop several ticks' worth of virtual time.
        tokio::time::sleep(std::time::Duration::from_secs(120)).await;
        assert!(
            rx.try_recv().is_err(),
            "the poll's copy of the relay-emitted action double-emitted after restart"
        );
        // Its durable decision is an observation, so the feed id joins
        // the cursor.
        let loaded = open_log(&dir).load();
        assert!(loaded.feed_ids.iter().any(|id| id == "77"));
        run2.abort();
        let _ = run2.await;
    }
}
