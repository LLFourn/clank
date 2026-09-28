//! The events feed: what an event MEANS, and the cursor that decides
//! which of them are new.
//!
//! Both classifiers are PURE and fixture-tested, and they agree: the
//! same underlying action yields the same action key whether it
//! arrived as an `/events` row or as a webhook payload. The transport
//! is INJECTED (`EventFetcher`) so the whole poll state machine is
//! driven deterministically in tests, and the production fetcher is
//! an async, cancellation-safe `gh` child (killed on task abort).
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
use std::time::Duration;

use clank_core::agent_config::{GithubEventKind, GithubSource};
use clank_core::wait::WaitItem;

use super::session::{GithubSession, HttpSend};

/// GitHub's `/events` feed caps at ~300 events (3 pages of 100).
pub(super) const MAX_PAGES: u32 = 3;
/// Bound the seen-ID set so a long-lived poll can't grow unbounded.
pub(super) const SEEN_CAP: usize = 1000;

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

/// One classified event, or `None` if it doesn't match a wanted
/// sub-kind, is filtered by actor, or is a non-creating action (an
/// edit/delete/dismiss is not a new comment/review). Pure.
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
                // Never at classification (github-watch-prompts):
                // the WAL stores no prompt; presentation stamps it.
                instructions: None,
                content: None,
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
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            actor: actor.map(str::to_string),
            url: subject_url(payload, obj),
            instructions: None,
            content: content_ref(kind, detail, payload, obj),
        },
        key,
        feed_id: None,
        event_at,
    })
}

/// The dedup namespace and payload container for a comment-family
/// event. Factored out of `action_key` so [`content_ref`] reads the
/// SAME fact instead of a second copy that can drift — the key's
/// output is unchanged by construction.
fn comment_ns_container(detail: Option<&str>) -> (&'static str, &'static str) {
    match detail {
        Some("review") => ("review", "review"),
        Some("review_comment") => ("review_comment", "comment"),
        Some("issue_comment_on_pr") => ("pr_issue_comment", "comment"),
        _ => ("issue_comment", "comment"),
    }
}

/// A non-empty `html_url` on this object, if it has one.
fn html_url(v: &serde_json::Value) -> Option<String> {
    v.get("html_url")
        .and_then(|u| u.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// The browser link for the object this event is ABOUT.
///
/// The container's link is the FALLBACK, not the answer: a comment
/// carries its own anchored `html_url`, which opens the thing the
/// event announced instead of the top of a long thread.
///
/// Shared by both classifiers on purpose. They see different payloads
/// — the Events API sends a `pull_request` stub of five keys with no
/// `html_url` at all, a webhook sends the full object — and two copies
/// of this decision is how the poll came to record no link for every
/// review comment while the webhook recorded its PR's.
///
/// Only `html_url` is ever read. The stub's `url` is an
/// `api.github.com` address, which is not something an operator can
/// open, so the absence of a link is the honest answer there.
fn subject_url(payload: &serde_json::Value, obj: &serde_json::Value) -> Option<String> {
    payload
        .get("comment")
        .and_then(html_url)
        .or_else(|| payload.get("review").and_then(html_url))
        .or_else(|| html_url(obj))
}

/// Which object carries this event's body
/// (tui-github-event-content). Derived from the payload, never from
/// `action_key`: the dedup string's shape must not become
/// load-bearing anywhere else.
fn content_ref(
    kind: GithubEventKind,
    detail: Option<&str>,
    payload: &serde_json::Value,
    obj: &serde_json::Value,
) -> Option<clank_core::wait::ContentRef> {
    use clank_core::wait::ContentRef;
    let number = obj.get("number").and_then(|n| n.as_u64());
    match kind {
        GithubEventKind::PrOpened | GithubEventKind::PrUpdated | GithubEventKind::PrMerged => {
            Some(ContentRef::Pr { number: number? })
        }
        GithubEventKind::IssueOpened | GithubEventKind::IssueClosed => {
            Some(ContentRef::Issue { number: number? })
        }
        GithubEventKind::PrComment | GithubEventKind::IssueComment => {
            let (ns, container) = comment_ns_container(detail);
            let id = payload
                .get(container)
                .and_then(|c| c.get("id"))
                .and_then(|i| i.as_u64())?;
            Some(match ns {
                // A review is addressed THROUGH its PR, so a missing
                // number makes it unaddressable — no reference rather
                // than a wrong one.
                "review" => ContentRef::Review {
                    number: number?,
                    id,
                },
                "review_comment" => ContentRef::ReviewComment { id },
                "pr_issue_comment" => ContentRef::PrIssueComment { id },
                _ => ContentRef::IssueComment { id },
            })
        }
        // A push has no single object to read a body from.
        GithubEventKind::BranchPush => None,
        // Not classified from a payload: a CI conclusion is read from
        // a PR's checks, never from the feed or a webhook body
        // (`github_ci`).
        GithubEventKind::CiPassed | GithubEventKind::CiFailed => {
            unreachable!("ci kinds are not classified from a payload")
        }
    }
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
            let (ns, container) = comment_ns_container(detail);
            let id = payload
                .get(container)
                .and_then(|c| c.get("id"))
                .and_then(|i| i.as_u64())?;
            Some(format!("{ns}#{id}"))
        }
        GithubEventKind::BranchPush => unreachable!("push keys are built in the push arms"),
        GithubEventKind::CiPassed | GithubEventKind::CiFailed => {
            unreachable!("ci keys are built by the ci reader, which has no payload")
        }
    }
}

pub(crate) fn kind_str(kind: GithubEventKind) -> &'static str {
    match kind {
        GithubEventKind::PrOpened => "pr_opened",
        GithubEventKind::PrUpdated => "pr_updated",
        GithubEventKind::BranchPush => "branch_push",
        GithubEventKind::PrMerged => "pr_merged",
        GithubEventKind::PrComment => "pr_comment",
        GithubEventKind::IssueOpened => "issue_opened",
        GithubEventKind::IssueClosed => "issue_closed",
        GithubEventKind::IssueComment => "issue_comment",
        GithubEventKind::CiPassed => "ci_passed",
        GithubEventKind::CiFailed => "ci_failed",
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
                // Never at classification (github-watch-prompts):
                // the WAL stores no prompt; presentation stamps it.
                instructions: None,
                content: None,
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
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            actor: actor.map(str::to_string),
            url: subject_url(payload, obj),
            instructions: None,
            content: content_ref(kind, detail, payload, obj),
        },
        key,
        feed_id: None,
        event_at,
    })
}

/// New items from one page (ids not in `seen`, matching + filtered),
/// plus the ids observed on the page. Pure.
pub(super) fn map_page(
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
        ) && in_scope(src, &classified.item)
        {
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
    pub(super) fn new(cap: usize) -> Self {
        Self {
            set: HashSet::new(),
            order: VecDeque::new(),
            cap,
        }
    }
    pub(super) fn as_btree(&self) -> BTreeSet<String> {
        self.set.iter().cloned().collect()
    }
    pub(super) fn extend(&mut self, ids: impl IntoIterator<Item = String>) {
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
    pub(super) fn len(&self) -> usize {
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

/// Production fetcher: the events endpoint over the SHARED session.
pub(super) struct GhFetcher<S: HttpSend> {
    pub(super) repo: String,
    pub(super) session: std::sync::Arc<GithubSession<S>>,
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

/// Is this item in scope for this watch?
///
/// ONE rule, applied wherever an item reaches the agent — both
/// classifiers, the warm backlog replay, and the follower's tail.
///
/// Two reasons it cannot live in the classifiers alone. The standing
/// one: the replay paths never classify anything, they send SAVED
/// rows, so a rule that only filters arrivals does not govern them
/// at all. And the one this plan created: a watch whose config did
/// not change keeps its source key across this upgrade, while its
/// log holds comments on other PRs that were legitimately accepted
/// when `prs` scoped CI alone (codex 8043f14).
///
/// Editing the list is NOT that case — `source_key` hashes a
/// non-empty `prs`, so a changed list is a different log.
///
/// Works on the ITEM, because a saved row is all the replay paths
/// have.
pub(crate) fn in_scope(src: &GithubSource, item: &WaitItem) -> bool {
    if src.prs.is_empty() {
        return true;
    }
    let WaitItem::GithubEvent { event, number, .. } = item else {
        return true;
    };
    // A kind the scope does not govern — `pr_opened`, the issue
    // kinds, `branch_push` — passes untouched, as does a kind this
    // binary does not know.
    match clank_core::agent_config::GithubEventKind::from_wire(event) {
        Some(kind) if kind.is_pr_scoped() => match number {
            Some(n) => src.prs.contains(n),
            None => {
                // An eligible event we cannot place. Dropping it
                // silently is the failure a scope makes hard to
                // notice, so it goes loudly — unlike an out-of-scope
                // PR, which is the scope working.
                eprintln!(
                    "wait: github {} — a {event} event carried no PR number; \
                     dropped, since `prs` cannot place it",
                    src.repo
                );
                false
            }
        },
        _ => true,
    }
}

/// The FEED's half of a watch: its cursor over the event stream, and
/// whether it has a horizon at all.
///
/// A type rather than four locals beside CI's four, because every
/// finding on this plan bar one was two of those locals being taken
/// for each other — a shared "warm", a shared key set, a shared save
/// (codex 7c132ec, ae48e92, 4b6035b, 4a6b328). What a loaded log
/// means for the feed is now one pure function that can be asked
/// directly, instead of a rule that only a two-arm async test can
/// reach.
pub(super) struct FeedHorizon {
    /// Bounded recency window over feed ids — right for a stream,
    /// where an id that falls out will not arrive again.
    pub(super) seen: SeenSet,
    pub(super) etag: Option<String>,
    /// The server's requested minimum interval, remembered across
    /// ticks. Not part of the horizon: a rate limit still applies to
    /// a source that has to re-baseline.
    pub(super) floor: Option<u64>,
    /// Has a baseline fetch succeeded? Until it has, what the feed
    /// returns is history rather than news.
    pub(super) established: bool,
}

impl FeedHorizon {
    /// What a loaded log means for the feed. Pure: the quarantine's
    /// durable invalidation is the caller's, since it writes.
    pub(super) fn restore(loaded: &crate::cli::github::log::LoadedLog) -> Self {
        let mut seen = SeenSet::new(SEEN_CAP);
        seen.extend(loaded.feed_ids.iter().cloned());

        // Did the FEED baseline? It records that itself; a reader
        // only has to decide what an OLD file means, where the field
        // is absent — and what a REJECTED log means, whatever the
        // file says.
        //
        // Absent, the answer is the rule that predates the field — a
        // valid WAL — minus the one state it cannot distinguish: a
        // log holding ONLY checks rows, which is evidence about the
        // checks and none about the feed. An empty feed baseline
        // followed by a relay delivery leaves exactly such a log
        // except that its rows are the feed's, so the question is
        // whose rows they are, not whether any exist.
        let evidence = loaded.warm
            && (!loaded.feed_ids.is_empty()
                || loaded
                    .action_keys
                    .iter()
                    .any(|k| !crate::cli::github::ci::is_ci_key(k)));
        // A quarantined log answers for nothing: whatever the sidecar
        // claims, the horizon it was written alongside is gone.
        let established = !loaded.log_quarantined && loaded.feed_baselined.unwrap_or(evidence);

        Self {
            seen,
            // Conditional-request metadata belongs to the horizon it
            // was taken against: an ETag restored for a feed that has
            // not established makes every fetch a 304, so the
            // baseline never completes and pre-baseline relay
            // deliveries buffer for good.
            etag: established.then(|| loaded.etag.clone()).flatten(),
            floor: established.then_some(loaded.poll_interval_floor).flatten(),
            established,
        }
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use std::sync::Mutex;

    use crate::cli::github::session::fetch_authenticated_login;
    use crate::cli::github::session::tests::{ACQUIRES, ScriptHttp, counting_acquire};

    pub(in crate::cli::github) fn src(
        events: &[GithubEventKind],
        include_own: bool,
    ) -> GithubSource {
        GithubSource {
            repo: "o/r".into(),
            events: events.to_vec(),
            poll_interval: None,
            include_own_actions: include_own,
            branches: Vec::new(),
            prs: Vec::new(),
            prompt: None,
            delivery: clank_core::agent_config::Delivery::Poll,
        }
    }

    /// A recorded-shape `/events` element.
    pub(in crate::cli::github) fn ev(
        id: &str,
        ty: &str,
        payload: serde_json::Value,
        actor: &str,
    ) -> serde_json::Value {
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
            serde_json::from_str(include_str!("events_fixture.json")).unwrap();
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
            prs: Vec::new(),
            delivery: clank_core::agent_config::Delivery::Poll,
            prompt: None,
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

    /// THE SUBJECT'S OWN LINK, for each transport's REAL payload.
    ///
    /// The two classifiers see different shapes, so a fixture from one
    /// proves nothing about the other. The poll reads the Events API,
    /// whose `pull_request` for a review-comment event is five keys —
    /// `base, head, id, number, url` — with no `html_url` at all; a
    /// webhook sends the full object. Feeding a full PR to the poll's
    /// classifier is what let `url: null` go unnoticed on every
    /// poll-sourced review comment.
    #[test]
    fn an_event_links_to_the_object_it_announces() {
        let all = &[GithubEventKind::PrComment];
        // The Events API stub, verbatim in shape.
        let stub = serde_json::json!({
            "base": {}, "head": {}, "id": 1, "number": 602,
            "url": "https://api.github.com/repos/o/r/pulls/602"
        });
        let poll = serde_json::json!({
            "type": "PullRequestReviewCommentEvent",
            "actor": {"login": "x"},
            "payload": {
                "action": "created",
                "comment": {"id": 7, "html_url": "https://github.com/o/r/pull/602#discussion_r7"},
                "pull_request": stub,
            }
        });
        let got = classify_event("o/r", &poll, all, Some("me"), false, &[]).expect("classified");
        let WaitItem::GithubEvent { url, title, .. } = &got.item else {
            panic!("not a github event")
        };
        assert_eq!(
            url.as_deref(),
            Some("https://github.com/o/r/pull/602#discussion_r7"),
            "the poll must record the comment's anchored link, not the stub's api url",
        );
        assert_eq!(
            title.as_deref(),
            None,
            "the stub carries no title to record"
        );

        // The webhook's full pull_request: the comment still wins.
        let hook = serde_json::json!({
            "action": "created",
            "sender": {"login": "x"},
            "comment": {"id": 7, "html_url": "https://github.com/o/r/pull/602#discussion_r7"},
            "pull_request": {
                "number": 602, "title": "t",
                "html_url": "https://github.com/o/r/pull/602"
            }
        });
        let got = classify_webhook(
            "o/r",
            "pull_request_review_comment",
            &hook,
            all,
            Some("me"),
            false,
            &[],
        )
        .expect("classified");
        let WaitItem::GithubEvent { url, title, .. } = &got.item else {
            panic!("not a github event")
        };
        assert_eq!(
            url.as_deref(),
            Some("https://github.com/o/r/pull/602#discussion_r7"),
            "the anchored comment beats the container, on this transport too",
        );
        assert_eq!(title.as_deref(), Some("t"));

        // A review carries its own link under a different key.
        let review = serde_json::json!({
            "action": "submitted",
            "sender": {"login": "x"},
            "review": {"id": 9, "html_url": "https://github.com/o/r/pull/587#pullrequestreview-9"},
            "pull_request": {"number": 587, "html_url": "https://github.com/o/r/pull/587"}
        });
        let got = classify_webhook(
            "o/r",
            "pull_request_review",
            &review,
            all,
            Some("me"),
            false,
            &[],
        )
        .expect("classified");
        let WaitItem::GithubEvent { url, .. } = &got.item else {
            panic!("not a github event")
        };
        assert_eq!(
            url.as_deref(),
            Some("https://github.com/o/r/pull/587#pullrequestreview-9"),
        );

        // No object link: the container is the fallback, and a payload
        // with neither records NOTHING rather than an api address.
        let bare = serde_json::json!({
            "type": "PullRequestReviewCommentEvent",
            "actor": {"login": "x"},
            "payload": {
                "action": "created",
                "comment": {"id": 7},
                "pull_request": stub,
            }
        });
        let got = classify_event("o/r", &bare, all, Some("me"), false, &[]).expect("classified");
        let WaitItem::GithubEvent { url, .. } = &got.item else {
            panic!("not a github event")
        };
        assert_eq!(
            url.as_deref(),
            None,
            "an api.github.com url is not a link an operator can open",
        );

        // EMPTY is not a link. GitHub returning "" for either field
        // would otherwise record a url that opens nothing and a title
        // that renders as a blank, which read as content rather than
        // as absence.
        let empty = serde_json::json!({
            "action": "created",
            "sender": {"login": "x"},
            "comment": {"id": 7, "html_url": ""},
            "pull_request": {
                "number": 602, "title": "",
                "html_url": "https://github.com/o/r/pull/602"
            }
        });
        let got = classify_webhook(
            "o/r",
            "pull_request_review_comment",
            &empty,
            all,
            Some("me"),
            false,
            &[],
        )
        .expect("classified");
        let WaitItem::GithubEvent { url, title, .. } = &got.item else {
            panic!("not a github event")
        };
        assert_eq!(
            url.as_deref(),
            Some("https://github.com/o/r/pull/602"),
            "an empty comment link falls through to the container",
        );
        assert_eq!(
            title.as_deref(),
            None,
            "an empty title is absent, not blank"
        );
    }

    /// THE REST OF THE FAMILIES, at their real entry points.
    ///
    /// Each kind reaches the shared rule down its own branch and reads
    /// its own payload key, so the review-comment case above proves
    /// none of them. A submitted review is the one that has to be
    /// driven on the POLL as well: that is where the `pull_request` is
    /// the five-key stub, so the container cannot cover for a missing
    /// `payload.review.html_url`.
    #[test]
    fn every_comment_family_links_to_its_own_object() {
        let all = &[GithubEventKind::PrComment, GithubEventKind::IssueComment];
        let url_of = |c: &Classified| {
            let WaitItem::GithubEvent { url, .. } = &c.item else {
                panic!("not a github event")
            };
            url.clone()
        };
        let stub = serde_json::json!({
            "base": {}, "head": {}, "id": 1, "number": 587,
            "url": "https://api.github.com/repos/o/r/pulls/587"
        });

        // A submitted review, on the POLL, against the real stub.
        let poll_review = serde_json::json!({
            "type": "PullRequestReviewEvent",
            "actor": {"login": "x"},
            "payload": {
                "action": "created",
                "review": {"id": 9, "html_url": "https://github.com/o/r/pull/587#pullrequestreview-9"},
                "pull_request": stub,
            }
        });
        assert_eq!(
            url_of(
                &classify_event("o/r", &poll_review, all, Some("me"), false, &[])
                    .expect("classified")
            ),
            Some("https://github.com/o/r/pull/587#pullrequestreview-9".into()),
            "a poll review must use its own link; the stub has none to fall back on",
        );

        // An issue comment, on the poll. The container here is a FULL
        // issue, so the anchored link has to beat a link that exists.
        let issue = serde_json::json!({
            "number": 12, "title": "t",
            "html_url": "https://github.com/o/r/issues/12"
        });
        let poll_issue = serde_json::json!({
            "type": "IssueCommentEvent",
            "actor": {"login": "x"},
            "payload": {
                "action": "created",
                "comment": {"id": 5, "html_url": "https://github.com/o/r/issues/12#issuecomment-5"},
                "issue": issue,
            }
        });
        assert_eq!(
            url_of(
                &classify_event("o/r", &poll_issue, all, Some("me"), false, &[])
                    .expect("classified")
            ),
            Some("https://github.com/o/r/issues/12#issuecomment-5".into()),
            "the comment's anchor, not the top of the thread",
        );

        // The same family on the webhook.
        let hook_issue = serde_json::json!({
            "action": "created",
            "sender": {"login": "x"},
            "comment": {"id": 5, "html_url": "https://github.com/o/r/issues/12#issuecomment-5"},
            "issue": issue,
        });
        assert_eq!(
            url_of(
                &classify_webhook(
                    "o/r",
                    "issue_comment",
                    &hook_issue,
                    all,
                    Some("me"),
                    false,
                    &[]
                )
                .expect("classified")
            ),
            Some("https://github.com/o/r/issues/12#issuecomment-5".into()),
        );

        // A comment on a PR's conversation tab is the same resource
        // and must answer the same way.
        let pr_issue = serde_json::json!({
            "number": 12, "title": "t", "pull_request": {},
            "html_url": "https://github.com/o/r/pull/12"
        });
        let poll_pr_issue = serde_json::json!({
            "type": "IssueCommentEvent",
            "actor": {"login": "x"},
            "payload": {
                "action": "created",
                "comment": {"id": 5, "html_url": "https://github.com/o/r/pull/12#issuecomment-5"},
                "issue": pr_issue,
            }
        });
        assert_eq!(
            url_of(
                &classify_event("o/r", &poll_pr_issue, all, Some("me"), false, &[])
                    .expect("classified")
            ),
            Some("https://github.com/o/r/pull/12#issuecomment-5".into()),
        );
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
    fn classification_actually_persists_the_reference() {
        // The helper being right proves nothing if the classifier does
        // not call it. Drive the REAL entry point and read the field
        // back off the produced item.
        use clank_core::wait::ContentRef;
        let all = &[GithubEventKind::PrComment, GithubEventKind::PrOpened][..];
        let got = |event: &str, payload: serde_json::Value| {
            classify_webhook("o/r", event, &payload, all, Some("me"), false, &[]).map(|c| {
                match c.item {
                    WaitItem::GithubEvent { content, .. } => content,
                    other => panic!("unexpected {other:?}"),
                }
            })
        };

        assert_eq!(
            got(
                "pull_request_review_comment",
                serde_json::json!({
                    "action": "created",
                    "pull_request": {"number": 42, "title": "t", "html_url": "u"},
                    "comment": {"id": 900}
                })
            ),
            Some(Some(ContentRef::ReviewComment { id: 900 }))
        );
        assert_eq!(
            got(
                "pull_request_review",
                serde_json::json!({
                    "action": "submitted",
                    "pull_request": {"number": 42, "title": "t", "html_url": "u"},
                    "review": {"id": 901}
                })
            ),
            Some(Some(ContentRef::Review {
                number: 42,
                id: 901
            }))
        );
        assert_eq!(
            got(
                "pull_request",
                serde_json::json!({
                    "action": "opened",
                    "pull_request": {"number": 42, "title": "t", "html_url": "u"}
                })
            ),
            Some(Some(ContentRef::Pr { number: 42 }))
        );
    }

    #[test]
    fn content_refs_address_each_comment_family_and_carry_what_the_endpoint_takes() {
        use clank_core::wait::ContentRef;
        let comment = |id: u64| serde_json::json!({"comment": {"id": id}});
        let review = |id: u64| serde_json::json!({"review": {"id": id}});
        let obj = serde_json::json!({"number": 42});

        let cases: Vec<(GithubEventKind, Option<&str>, serde_json::Value, ContentRef)> = vec![
            (
                GithubEventKind::IssueComment,
                None,
                comment(7),
                ContentRef::IssueComment { id: 7 },
            ),
            (
                GithubEventKind::PrComment,
                Some("issue_comment_on_pr"),
                comment(7),
                ContentRef::PrIssueComment { id: 7 },
            ),
            (
                GithubEventKind::PrComment,
                Some("review_comment"),
                comment(7),
                ContentRef::ReviewComment { id: 7 },
            ),
            (
                // The one addressed THROUGH its PR: same id, and the
                // number is part of the address rather than decoration.
                GithubEventKind::PrComment,
                Some("review"),
                review(7),
                ContentRef::Review { number: 42, id: 7 },
            ),
            (
                GithubEventKind::PrOpened,
                None,
                serde_json::json!({}),
                ContentRef::Pr { number: 42 },
            ),
            (
                GithubEventKind::IssueOpened,
                None,
                serde_json::json!({}),
                ContentRef::Issue { number: 42 },
            ),
        ];
        for (kind, detail, payload, want) in cases {
            assert_eq!(
                content_ref(kind, detail, &payload, &obj),
                Some(want.clone()),
                "{kind:?}/{detail:?} must address {want:?}"
            );
        }

        // Endpoints differ per family — the whole reason the variants
        // are distinct.
        assert_eq!(
            ContentRef::IssueComment { id: 7 }.path("o/r"),
            "/repos/o/r/issues/comments/7"
        );
        assert_eq!(
            ContentRef::ReviewComment { id: 7 }.path("o/r"),
            "/repos/o/r/pulls/comments/7"
        );
        assert_eq!(
            ContentRef::Review { number: 42, id: 7 }.path("o/r"),
            "/repos/o/r/pulls/42/reviews/7"
        );
        assert_eq!(
            ContentRef::Pr { number: 42 }.path("o/r"),
            "/repos/o/r/pulls/42"
        );
        assert_eq!(
            ContentRef::Issue { number: 42 }.path("o/r"),
            "/repos/o/r/issues/42"
        );
    }

    #[test]
    fn an_unaddressable_object_yields_no_reference_rather_than_a_wrong_one() {
        // Same rule action_key already follows: never manufacture an
        // identity. A review with no PR number cannot be addressed, and
        // a comment with no id cannot either.
        let no_number = serde_json::json!({});
        assert_eq!(
            content_ref(
                GithubEventKind::PrComment,
                Some("review"),
                &serde_json::json!({"review": {"id": 7}}),
                &no_number,
            ),
            None,
            "a review needs its PR number to be addressable"
        );
        assert_eq!(
            content_ref(
                GithubEventKind::IssueComment,
                None,
                &serde_json::json!({"comment": {}}),
                &serde_json::json!({"number": 42}),
            ),
            None,
            "no comment id → no reference"
        );
        assert_eq!(
            content_ref(
                GithubEventKind::BranchPush,
                None,
                &serde_json::json!({}),
                &no_number,
            ),
            None,
            "a push has no single body to read"
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

    // ── injected-runner poll state machine ─────────────────────

    pub(in crate::cli::github) type Step =
        Box<dyn Fn(u32, Option<String>) -> anyhow::Result<GhResponse> + Send>;

    /// A scripted fetcher: a queue of per-`fetch(page, etag)` responses
    /// (each a closure so a test can assert the conditional ETag) and a
    /// log of the (page, etag) calls. When the script is exhausted it
    /// returns `304` forever, so a real `poll_loop` can keep ticking.
    pub(in crate::cli::github) struct ScriptFetcher {
        script: Mutex<VecDeque<Step>>,
        pub(in crate::cli::github) calls: Mutex<Vec<(u32, Option<String>)>>,
        /// Paused-clock instants of each fetch — lets a test assert the
        /// interval the loop actually slept (tokio time is
        /// deterministic under start_paused).
        pub(in crate::cli::github) instants: Mutex<Vec<tokio::time::Instant>>,
    }

    impl ScriptFetcher {
        pub(in crate::cli::github) fn new(steps: Vec<Step>) -> Self {
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

    pub(in crate::cli::github) fn ok(
        events: Vec<serde_json::Value>,
        etag: Option<&str>,
        pi: Option<u64>,
    ) -> GhResponse {
        GhResponse::Ok {
            events,
            etag: etag.map(str::to_string),
            poll_interval: pi,
        }
    }

    pub(in crate::cli::github) fn issue(id: &str, n: u64) -> serde_json::Value {
        ev(
            id,
            "IssuesEvent",
            serde_json::json!({"action":"opened","issue":{"number":n}}),
            "x",
        )
    }

    pub(in crate::cli::github) fn seen_of(ids: &[&str]) -> BTreeSet<String> {
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

    #[test]
    fn the_feed_page_is_filtered_by_the_scope() {
        // The classification path: a page carrying comments on two
        // PRs yields only the one in scope.
        let mut s = src(&[GithubEventKind::PrComment], true);
        s.prs = vec![7];
        let page: Vec<serde_json::Value> = [7u64, 8]
            .iter()
            .map(|n| {
                ev(
                    &format!("c{n}"),
                    "IssueCommentEvent",
                    serde_json::json!({
                        "action": "created",
                        "issue": {"number": n, "pull_request": {}},
                        "comment": {"id": n * 100}
                    }),
                    "them",
                )
            })
            .collect();
        let (items, ids) = map_page(&s, &page, &BTreeSet::new(), None);
        assert_eq!(ids.len(), 2, "both ids are still cursor-observed");
        let numbers: Vec<Option<u64>> = items
            .iter()
            .map(|c| match &c.item {
                WaitItem::GithubEvent { number, .. } => *number,
                _ => None,
            })
            .collect();
        assert_eq!(numbers, vec![Some(7)], "only the watched PR is presented");
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

    // ── what a loaded log means for the feed ─────────────────
    //
    // Every one of these was reachable before only through a two-arm
    // async test with a temp dir, which is why three of them were
    // wrong at once.

    pub(in crate::cli::github) fn loaded(warm: bool) -> crate::cli::github::log::LoadedLog {
        crate::cli::github::log::LoadedLog {
            warm,
            ..Default::default()
        }
    }

    #[test]
    fn a_cold_log_has_no_horizon_and_no_etag() {
        let h = FeedHorizon::restore(&loaded(false));
        assert!(!h.established);
        assert_eq!(h.etag, None);
    }

    #[test]
    fn the_recorded_fact_decides_when_it_is_there() {
        let mut l = loaded(true);
        l.feed_baselined = Some(true);
        l.etag = Some("\"e\"".into());
        let h = FeedHorizon::restore(&l);
        assert!(
            h.established,
            "even with no rows at all — an empty baseline"
        );
        assert_eq!(h.etag.as_deref(), Some("\"e\""));

        l.feed_baselined = Some(false);
        l.feed_ids = vec!["1".into()];
        let h = FeedHorizon::restore(&l);
        assert!(!h.established, "an explicit no outranks the rows");
        assert_eq!(h.etag, None, "and the etag goes with the horizon");
    }

    #[test]
    fn an_old_file_is_answered_by_whose_rows_the_log_holds() {
        // No flag at all: the pre-field rule, minus checks-only rows.
        let mut l = loaded(true);
        l.feed_ids = vec!["1".into()];
        assert!(
            FeedHorizon::restore(&l).established,
            "feed ids are the feed's"
        );

        let mut l = loaded(true);
        l.action_keys = vec!["issue_opened#4".into()];
        assert!(
            FeedHorizon::restore(&l).established,
            "a relay row is the feed's too — an empty baseline then a webhook",
        );

        let mut l = loaded(true);
        l.action_keys = vec!["ci_failed#7@abc".into(), "ci_passed#9@def".into()];
        assert!(
            !FeedHorizon::restore(&l).established,
            "a checks-only log says nothing about the feed",
        );
    }

    #[test]
    fn a_quarantined_log_answers_for_the_cache_beside_it() {
        let mut l = loaded(false);
        l.log_quarantined = true;
        l.feed_baselined = Some(true);
        l.etag = Some("\"stale\"".into());
        l.poll_interval_floor = Some(60);
        let h = FeedHorizon::restore(&l);
        assert!(!h.established, "the horizon it was written beside is gone");
        assert_eq!(h.etag, None, "so the conditional request goes with it");
        assert_eq!(h.floor, None);
    }

    #[test]
    fn the_feed_ids_become_the_cursor() {
        let mut l = loaded(true);
        l.feed_ids = vec!["1".into(), "2".into()];
        let h = FeedHorizon::restore(&l);
        assert!(h.seen.as_btree().contains("1"));
        assert!(h.seen.as_btree().contains("2"));
    }

    pub(in crate::cli::github) fn gh_item(event: &str, number: Option<u64>) -> WaitItem {
        WaitItem::GithubEvent {
            repo: "o/r".into(),
            event: event.into(),
            detail: None,
            number,
            title: None,
            actor: None,
            url: None,
            instructions: None,
            content: None,
        }
    }

    #[test]
    fn the_scope_governs_the_kinds_that_name_a_pr() {
        let mut s = src(&[GithubEventKind::PrComment], true);
        s.prs = vec![7];
        for kind in [
            "pr_comment",
            "pr_updated",
            "pr_merged",
            "ci_failed",
            "ci_passed",
        ] {
            assert!(in_scope(&s, &gh_item(kind, Some(7))), "{kind} on 7");
            assert!(!in_scope(&s, &gh_item(kind, Some(8))), "{kind} on 8");
        }
    }

    #[test]
    fn the_kinds_that_name_no_pr_are_untouched() {
        let mut s = src(&[GithubEventKind::PrOpened], true);
        s.prs = vec![7];
        // A PR that does not exist yet cannot be in a list written
        // before it, so `pr_opened` is outside the scope by design.
        assert!(in_scope(&s, &gh_item("pr_opened", Some(8))));
        for kind in [
            "issue_opened",
            "issue_closed",
            "issue_comment",
            "branch_push",
        ] {
            assert!(in_scope(&s, &gh_item(kind, Some(8))), "{kind}");
            assert!(
                in_scope(&s, &gh_item(kind, None)),
                "{kind} without a number"
            );
        }
        // A kind this binary does not know passes rather than
        // vanishing: the scope is not a place to lose events.
        assert!(in_scope(&s, &gh_item("telepathy", Some(8))));
    }

    #[test]
    fn an_empty_scope_is_every_pr() {
        let s = src(&[GithubEventKind::PrComment], true);
        assert!(s.prs.is_empty());
        assert!(in_scope(&s, &gh_item("pr_comment", Some(8))));
        assert!(in_scope(&s, &gh_item("pr_comment", None)));
    }

    #[test]
    fn a_scoped_event_with_no_number_cannot_be_placed() {
        let mut s = src(&[GithubEventKind::PrComment], true);
        s.prs = vec![7];
        assert!(
            !in_scope(&s, &gh_item("pr_comment", None)),
            "it is dropped — loudly, since a scope that swallows events \
             silently is the thing that makes one hard to trust",
        );
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

    pub(in crate::cli::github) fn classified(n: u64, key: Option<&str>) -> Classified {
        Classified {
            item: WaitItem::GithubEvent {
                repo: "o/r".into(),
                event: "issue_opened".into(),
                detail: None,
                number: Some(n),
                title: None,
                actor: None,
                url: None,
                instructions: None,
                content: None,
            },
            key: key.map(str::to_string),
            feed_id: None,
            event_at: None,
        }
    }
}
