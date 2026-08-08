//! The event page's GitHub body: what state a fetch is in, which
//! completions are still wanted, and when a retry is worth offering
//! (tui-github-event-content, Contracts 3 and 4).
//!
//! Pure — no IO — so the rules that decide what the operator sees can
//! be driven directly instead of raced against a real request.

use clank_core::wait::ContentRef;

/// A fetched object's displayable content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EventBody {
    /// The markdown body. Empty is legitimate: GitHub allows a review
    /// or comment with no text, and that is not an error.
    pub(crate) body: String,
    pub(crate) author: Option<String>,
    /// The object's own canonical URL — for a comment this is the
    /// anchored link the event record never had.
    pub(crate) url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ContentState {
    Loading,
    Ready(EventBody),
    /// The object is gone (deleted, or invisible to this token).
    /// TERMINAL: retrying cannot bring it back, so the page must not
    /// offer to.
    Unavailable(String),
    /// Something that could plausibly differ next time. Only this
    /// state offers a retry.
    Failed(String),
}

impl ContentState {
    /// The gate on offering the retry action. Keeping this next to the
    /// states themselves is what stops "retryable" from drifting into
    /// a label the page prints but nothing honours.
    pub(crate) fn is_retryable(&self) -> bool {
        matches!(self, ContentState::Failed(_))
    }
}

/// Map an HTTP outcome to a state.
///
/// 404 and 410 are the only terminal cases: the object is deleted or
/// not visible, and no amount of retrying changes that. Everything
/// else non-2xx stays retryable ON PURPOSE — calling a recoverable
/// failure terminal strands the operator with no way to try again,
/// while an extra offered retry costs one request.
pub(crate) fn state_from_status(status: u16, body: &str) -> ContentState {
    match status {
        200..=299 => match parse_body(body) {
            Some(b) => ContentState::Ready(b),
            // A 2xx we cannot read is a BROKEN response, not an empty
            // comment (codex on 48239ce). Calling it Ready would show
            // blank content, drop the canonical URL, and — worst —
            // suppress the retry that would have fixed it.
            None => {
                ContentState::Failed("GitHub returned a response that could not be read".into())
            }
        },
        404 | 410 => ContentState::Unavailable(
            "no longer on GitHub (deleted, or not visible to this token)".into(),
        ),
        429 => ContentState::Failed("rate limited by GitHub".into()),
        s @ 500..=599 => ContentState::Failed(format!("GitHub returned {s}")),
        s => ContentState::Failed(format!("GitHub returned {s}")),
    }
}

/// A transport failure never says anything about the object, so it is
/// always retryable.
pub(crate) fn state_from_transport_error(msg: &str) -> ContentState {
    ContentState::Failed(format!("could not reach GitHub: {msg}"))
}

/// Decode a GitHub object response. `None` means the payload was not
/// one — truncated, an HTML proxy page, or any JSON that is not an
/// object — which the caller must treat as a failure rather than as
/// content.
fn parse_body(raw: &str) -> Option<EventBody> {
    let v: serde_json::Value = serde_json::from_str(raw).ok()?;
    // Every object endpoint here returns a JSON OBJECT. A bare array,
    // string, or null is a response we did not ask for, so refusing it
    // here is what keeps a broken read from masquerading as an empty
    // one.
    let v = v.as_object()?;
    let str_at = |k: &str| {
        v.get(k)
            .and_then(|x| x.as_str())
            .map(str::to_string)
            .filter(|s| !s.is_empty())
    };
    Some(EventBody {
        body: str_at("body").unwrap_or_default(),
        author: v
            .get("user")
            .and_then(|u| u.get("login"))
            .and_then(|l| l.as_str())
            .map(str::to_string),
        url: str_at("html_url"),
    })
}

/// The page's in-flight content request.
///
/// The generation is the whole staleness story: a completion is only
/// applied if it answers the request the page is CURRENTLY waiting
/// for. Retargeting or retrying mints a new generation, so a slow
/// response from before cannot land on whatever is open when it
/// finally arrives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ContentSlot {
    pub(crate) target: ContentRef,
    pub(crate) generation: u64,
    pub(crate) state: ContentState,
}

impl ContentSlot {
    pub(crate) fn requesting(target: ContentRef, generation: u64) -> Self {
        Self {
            target,
            generation,
            state: ContentState::Loading,
        }
    }

    /// Whether a completion answers the current request. BOTH halves
    /// matter: the generation catches a retry of the same object, and
    /// the reference catches a retarget that happens to reuse one.
    pub(crate) fn accepts(&self, generation: u64, target: &ContentRef) -> bool {
        self.generation == generation && &self.target == target
    }

    /// Apply a completion if it is still wanted. Returns whether it
    /// was taken, so a caller can tell "ignored a straggler" from
    /// "nothing arrived".
    pub(crate) fn apply(
        &mut self,
        generation: u64,
        target: &ContentRef,
        state: ContentState,
    ) -> bool {
        if !self.accepts(generation, target) {
            return false;
        }
        self.state = state;
        true
    }

    /// Re-request the same object under a fresh generation, which
    /// abandons any completion still in flight.
    pub(crate) fn retry(&mut self, generation: u64) {
        self.generation = generation;
        self.state = ContentState::Loading;
    }
}

/// The one call the page's content needs. A seam, so the fixtures
/// below run with no network and no `gh` on PATH — the reason to
/// reuse the injectable transport rather than shell a subprocess per
/// view.
pub(crate) trait ContentFetch {
    /// GET an api.github.com path, yielding (status, body) or a
    /// transport-level failure message.
    fn get_path(
        &self,
        path: &str,
    ) -> impl std::future::Future<Output = Result<(u16, String), String>> + Send;
}

/// Fetch one object and classify the outcome. All the decision-making
/// lives in the pure functions above; this only chooses which of them
/// the result belongs to.
pub(crate) async fn fetch_content<F: ContentFetch>(
    fetcher: &F,
    repo: &str,
    target: &ContentRef,
) -> ContentState {
    match fetcher.get_path(&target.path(repo)).await {
        Ok((status, body)) => state_from_status(status, &body),
        Err(e) => state_from_transport_error(&e),
    }
}

/// Production fetcher: the SHARED github session, so the event page
/// reuses the process's token cell and its 401 handling instead of
/// standing up a second authorized path.
pub(crate) struct SessionFetch(
    pub(crate)  std::sync::Arc<
        crate::cli::github_events::GithubSession<crate::cli::github_events::ReqwestSend>,
    >,
);

impl ContentFetch for SessionFetch {
    async fn get_path(&self, path: &str) -> Result<(u16, String), String> {
        let url = format!("https://api.github.com{path}");
        match self.0.get(&url, None).await {
            Ok(r) => Ok((r.status, r.body)),
            Err(e) => Err(format!("{e:#}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(id: u64) -> ContentRef {
        ContentRef::ReviewComment { id }
    }

    /// Records what was requested and replies from a script — no
    /// network, no `gh`.
    struct FakeFetch {
        reply: Result<(u16, String), String>,
        asked: std::sync::Mutex<Vec<String>>,
    }

    impl FakeFetch {
        fn ok(status: u16, body: &str) -> Self {
            Self {
                reply: Ok((status, body.to_string())),
                asked: std::sync::Mutex::new(Vec::new()),
            }
        }
        fn err(msg: &str) -> Self {
            Self {
                reply: Err(msg.to_string()),
                asked: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    impl ContentFetch for FakeFetch {
        async fn get_path(&self, path: &str) -> Result<(u16, String), String> {
            self.asked.lock().unwrap().push(path.to_string());
            self.reply.clone()
        }
    }

    #[tokio::test]
    async fn each_reference_is_fetched_from_its_own_endpoint() {
        // The addresses are not interchangeable — this is what would
        // silently fetch the wrong object if a variant were dropped.
        let cases = [
            (
                ContentRef::IssueComment { id: 5 },
                "/repos/o/r/issues/comments/5",
            ),
            (
                ContentRef::PrIssueComment { id: 5 },
                "/repos/o/r/issues/comments/5",
            ),
            (
                ContentRef::ReviewComment { id: 5 },
                "/repos/o/r/pulls/comments/5",
            ),
            (
                ContentRef::Review { number: 9, id: 5 },
                "/repos/o/r/pulls/9/reviews/5",
            ),
            (ContentRef::Pr { number: 9 }, "/repos/o/r/pulls/9"),
            (ContentRef::Issue { number: 9 }, "/repos/o/r/issues/9"),
        ];
        for (target, want_path) in cases {
            let f = FakeFetch::ok(200, r#"{"body":"hi"}"#);
            let state = fetch_content(&f, "o/r", &target).await;
            assert_eq!(
                f.asked.lock().unwrap().as_slice(),
                &[want_path.to_string()],
                "{target:?} must be read from its own endpoint"
            );
            assert!(matches!(state, ContentState::Ready(_)));
        }
    }

    #[tokio::test]
    async fn fetch_outcomes_classify_the_same_way_the_pure_rules_do() {
        let gone = fetch_content(
            &FakeFetch::ok(404, ""),
            "o/r",
            &ContentRef::Pr { number: 1 },
        )
        .await;
        assert!(matches!(gone, ContentState::Unavailable(_)));
        assert!(!gone.is_retryable());

        let down = fetch_content(
            &FakeFetch::err("connection reset"),
            "o/r",
            &ContentRef::Pr { number: 1 },
        )
        .await;
        assert!(down.is_retryable(), "a transport failure stays retryable");
    }

    #[test]
    fn only_a_missing_object_is_terminal() {
        for gone in [404, 410] {
            let s = state_from_status(gone, "");
            assert!(matches!(s, ContentState::Unavailable(_)), "{gone}");
            assert!(!s.is_retryable(), "{gone} must not offer a retry forever");
        }
        for transient in [429, 500, 502, 503, 401, 403] {
            let s = state_from_status(transient, "");
            assert!(s.is_retryable(), "{transient} must stay retryable");
        }
        assert!(state_from_transport_error("dns").is_retryable());
    }

    #[test]
    fn a_body_is_parsed_and_an_empty_one_is_not_an_error() {
        let ready = state_from_status(
            200,
            r#"{"body":"line one\nline two","user":{"login":"octocat"},"html_url":"u#c1"}"#,
        );
        assert_eq!(
            ready,
            ContentState::Ready(EventBody {
                body: "line one\nline two".into(),
                author: Some("octocat".into()),
                url: Some("u#c1".into()),
            })
        );

        // GitHub allows an empty review body; that is content, not
        // failure, and must not be reported as an error.
        let empty = state_from_status(200, r#"{"body":null,"user":{"login":"o"}}"#);
        match empty {
            ContentState::Ready(b) => {
                assert_eq!(b.body, "");
                assert_eq!(b.url, None);
            }
            other => panic!("empty body must still be Ready, got {other:?}"),
        }
    }

    #[test]
    fn an_unreadable_2xx_is_a_failure_not_empty_content() {
        // The distinction that matters: a VALID object with no body is
        // content, but a response we could not decode is a broken read.
        // Conflating them shows a blank page, loses the canonical URL,
        // and hides the retry that would have fixed it.
        let valid_but_empty = state_from_status(200, r#"{"body":null}"#);
        assert!(
            matches!(valid_but_empty, ContentState::Ready(_)),
            "a valid object with a null body is content"
        );

        for broken in [
            r#"{"body":"truncat"#,                // cut off mid-response
            "<html><body>502 Bad Gateway</body>", // a proxy page, not JSON
            "[]",                                 // valid JSON, wrong shape
            "\"just a string\"",
            "null",
            "",
        ] {
            let s = state_from_status(200, broken);
            assert!(
                s.is_retryable(),
                "an unreadable 2xx must stay retryable, got {s:?} for {broken:?}"
            );
        }
    }

    #[test]
    fn a_completion_from_a_previous_generation_is_discarded() {
        // The bug this exists to prevent: a slow response landing on
        // whatever the page shows by the time it arrives.
        let mut slot = ContentSlot::requesting(r(1), 7);
        assert!(
            !slot.apply(6, &r(1), state_from_status(200, r#"{"body":"stale"}"#)),
            "an older generation must be refused"
        );
        assert_eq!(slot.state, ContentState::Loading, "state untouched");

        assert!(slot.apply(7, &r(1), state_from_status(200, r#"{"body":"fresh"}"#)));
        match &slot.state {
            ContentState::Ready(b) => assert_eq!(b.body, "fresh"),
            other => panic!("expected the current answer, got {other:?}"),
        }
    }

    #[test]
    fn a_completion_for_a_different_object_is_discarded() {
        // Retargeting can reuse a generation counter; the reference is
        // the other half of the identity.
        let mut slot = ContentSlot::requesting(r(1), 7);
        assert!(!slot.apply(7, &r(2), state_from_status(200, r#"{"body":"other"}"#)));
        assert_eq!(slot.state, ContentState::Loading);
    }

    #[test]
    fn retrying_abandons_the_request_it_replaces() {
        let mut slot = ContentSlot::requesting(r(1), 7);
        slot.apply(7, &r(1), state_from_status(500, ""));
        assert!(slot.state.is_retryable());

        slot.retry(8);
        assert_eq!(slot.state, ContentState::Loading);
        // The failed attempt finishing late must not resurrect itself
        // over the retry.
        assert!(!slot.apply(7, &r(1), state_from_status(500, "")));
        assert_eq!(slot.state, ContentState::Loading);
        assert!(slot.apply(8, &r(1), state_from_status(200, r#"{"body":"ok"}"#)));
    }
}
