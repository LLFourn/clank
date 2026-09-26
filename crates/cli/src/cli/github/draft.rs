//! A pending review's draft comments, read as state.
//!
//! Nothing DELIVERS a draft edit — measured against a real PR rather
//! than assumed (`the-review-session-watches-its-pr`): webhooks fire
//! for none of create/edit/delete, and the events feed carries the
//! creation only to retract it when the draft comment is deleted. So
//! the only honest channel is to re-read the whole draft and diff it
//! against what was last observed, which makes this the same shape as
//! [`super::ci`]: a complete read, a durable memory, and a change the
//! watch assigns its own revision to.

use std::collections::BTreeMap;

use clank_core::pr_review::DraftChange;

use super::session::{GithubSession, HttpSend, PER_PAGE, read_every_page};

/// One draft comment as one COMPLETE snapshot saw it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DraftComment {
    pub(crate) id: u64,
    pub(crate) path: String,
    pub(crate) body: String,
    /// Corroborating evidence, never the identity. GitHub documents
    /// this as a whole-second timestamp and guarantees nothing about
    /// two mutations receiving distinct values, so a reader keyed on
    /// `(id, updated_at)` would discard a changed body it actually
    /// observed (codex, fc7aaaf).
    pub(crate) updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct DraftTransition {
    /// The comment this is about — absent when the change is the
    /// whole draft going away.
    pub(crate) comment: Option<u64>,
    pub(crate) path: Option<String>,
    pub(crate) change: DraftChange,
    /// Assigned by this watch to each change it DETECTS, because
    /// GitHub offers no counter that can play the part.
    pub(crate) revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct Observed {
    path: String,
    body: String,
    updated_at: String,
}

impl From<&DraftComment> for Observed {
    fn from(c: &DraftComment) -> Self {
        Self {
            path: c.path.clone(),
            body: c.body.clone(),
            updated_at: c.updated_at.clone(),
        }
    }
}

/// Reading the draft is the whole channel, so the seam is injected
/// and the production reader is the only part that talks to github.
pub(crate) trait DraftReader {
    /// The PR's pending review, or `None` when master has not drafted
    /// one yet. A pending review is visible only to its author, so
    /// whichever one this sees is the team's own.
    fn pending_review(
        &self,
        pr: u32,
    ) -> impl std::future::Future<Output = anyhow::Result<Option<u64>>> + Send;

    fn read(
        &self,
        pr: u32,
        review: u64,
    ) -> impl std::future::Future<Output = anyhow::Result<Vec<DraftComment>>> + Send;
}

pub(super) struct GhDraft<S: HttpSend> {
    pub(super) repo: String,
    pub(super) session: std::sync::Arc<GithubSession<S>>,
}

/// Re-reads allowed before an unstable draft is refused rather than
/// guessed at.
const SETTLE_ATTEMPTS: u32 = 4;

impl<S: HttpSend> GhDraft<S> {
    async fn pages(&self, pr: u32, review: u64) -> anyhow::Result<Vec<DraftComment>> {
        let repo = &self.repo;
        let items = read_every_page(
            &self.session,
            |page| {
                format!(
                    "https://api.github.com/repos/{repo}/pulls/{pr}/reviews/{review}/comments\
                     ?per_page={PER_PAGE}&page={page}"
                )
            },
            None,
        )
        .await?;
        comments_from(&items)
    }
}

impl<S: HttpSend> DraftReader for GhDraft<S> {
    async fn pending_review(&self, pr: u32) -> anyhow::Result<Option<u64>> {
        let repo = &self.repo;
        let items = read_every_page(
            &self.session,
            |page| {
                format!(
                    "https://api.github.com/repos/{repo}/pulls/{pr}/reviews\
                     ?per_page={PER_PAGE}&page={page}"
                )
            },
            None,
        )
        .await?;
        Ok(items
            .iter()
            .find(|r| r.get("state").and_then(|s| s.as_str()) == Some("PENDING"))
            .and_then(|r| r.get("id").and_then(|i| i.as_u64())))
    }

    /// A snapshot, which is a stronger thing than a successful read.
    ///
    /// Offset pagination over a list the operator is editing is not a
    /// consistent view: delete a comment after page 1 comes back and
    /// every later row shifts down a page, so a comment that still
    /// exists is never fetched — and absence is how this watch reads
    /// deletion, so it would report the wrong comment deleted and then
    /// re-add it (codex 97355af, who built the case).
    ///
    /// One response IS a consistent view, so a draft that fits in a
    /// page needs nothing further, which is the ordinary case. Beyond
    /// that the only honest evidence is that the collection held
    /// still: re-read until two consecutive assemblies agree on WHICH
    /// comments exist, and refuse rather than guess if it never
    /// settles. Bodies may differ between those reads — that is an
    /// edit, and the last read carries it.
    async fn read(&self, pr: u32, review: u64) -> anyhow::Result<Vec<DraftComment>> {
        let mut previous: Option<std::collections::BTreeSet<u64>> = None;
        for _ in 0..SETTLE_ATTEMPTS {
            let rows = self.pages(pr, review).await?;
            if rows.len() < PER_PAGE {
                return Ok(rows);
            }
            let ids: std::collections::BTreeSet<u64> = rows.iter().map(|c| c.id).collect();
            if previous.as_ref() == Some(&ids) {
                return Ok(rows);
            }
            previous = Some(ids);
        }
        anyhow::bail!(
            "github {}/pulls/{pr}/reviews/{review}/comments: the draft changed under \
             {SETTLE_ATTEMPTS} consecutive reads — refusing to call any of them a snapshot",
            self.repo,
        )
    }
}

/// Every field is REQUIRED. A row missing its id would read as one
/// comment deleted and another added, and a row missing its body
/// would read as the operator blanking it — so a malformed row fails
/// the whole snapshot rather than entering the horizon, which is the
/// same rule that keeps a half-fetched page from presenting as
/// deletions.
fn comments_from(items: &[serde_json::Value]) -> anyhow::Result<Vec<DraftComment>> {
    items
        .iter()
        .map(|v| {
            let field = |name: &str| {
                v.get(name)
                    .and_then(|x| x.as_str())
                    .map(str::to_string)
                    .ok_or_else(|| anyhow::anyhow!("draft comment: no readable `{name}`"))
            };
            Ok(DraftComment {
                id: v
                    .get("id")
                    .and_then(|x| x.as_u64())
                    .ok_or_else(|| anyhow::anyhow!("draft comment: no readable `id`"))?,
                path: field("path")?,
                body: field("body")?,
                updated_at: field("updated_at")?,
            })
        })
        .collect()
}

/// One observation: find the pending review, read it whole, and say
/// what changed since the last observation.
///
/// The horizon advances IN MEMORY only. The caller stages the resulting
/// snapshot before appending its changes to the event log; a crash can
/// then finish the staged ingest on the next arm.
///
/// A read that failed, or a draft that would not hold still, leaves
/// the horizon untouched and reports nothing. Absence is how this
/// reads deletion, so a snapshot it does not trust must never become
/// one — and a CONFIRMED absence is a different thing from a failed
/// look, which is why only the first reaches the horizon.
pub(crate) async fn observe<R: DraftReader>(
    reader: &R,
    pr: u32,
    horizon: &mut DraftHorizon,
) -> anyhow::Result<Vec<DraftTransition>> {
    match reader.pending_review(pr).await? {
        None => Ok(horizon.absorb_absence()),
        Some(review) => {
            let snapshot = reader.read(pr, review).await?;
            Ok(horizon.absorb(review, snapshot))
        }
    }
}

/// The production reader: the one part of this that talks to github.
pub(crate) fn production_reader(repo: String) -> impl DraftReader {
    GhDraft {
        repo,
        session: super::session::shared_session(),
    }
}

/// What the observer has established about the review's draft.
///
/// Three states, because two would conflate the two ways there is
/// nothing to diff against: never having looked, and having looked
/// and found no draft (codex 6a69ca7). The pending review's identity
/// is part of the state, so a draft discarded and started again is a
/// new draft rather than a diff against a dead one's comments.
// Externally tagged, NOT `tag = "state"`: an internally tagged enum
// buffers through `serde_json::Value`, which stringifies the seen
// map's keys and then cannot read them back as ids.
#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum Draft {
    #[default]
    Unknown,
    Absent,
    Present {
        review: u64,
        seen: BTreeMap<u64, Observed>,
    },
}

/// What this watch has already seen of one review's draft.
///
/// Owned by the review's lifecycle rather than by a wait, so that it
/// spans the gaps: a horizon that re-baselined on each park would be
/// blind exactly while master is integrating a round (codex 2a26d72).
#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct DraftHorizon {
    revision: u64,
    draft: Draft,
    /// An observation staged before its event-log rows are appended.
    /// This is an ingest transaction, not delivery state: the ordinary
    /// event inbox owns every change until the agent explicitly acks it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pending: Option<DraftBatch>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct DraftBatch {
    revision: u64,
    draft: Draft,
    pub(crate) changes: Vec<DraftTransition>,
}

impl DraftHorizon {
    /// A horizon for a review that is beginning: it starts from no
    /// draft, so the first comment to appear is a change rather than
    /// a baseline. `Default` cannot say this — it means "never
    /// looked", which is what makes a late first read swallow
    /// whatever happened before it.
    pub(crate) fn started() -> Self {
        Self {
            draft: Draft::Absent,
            ..Self::default()
        }
    }

    /// Stage the observed state before any event-log append. A later
    /// source can finish this exact batch after a crash or handoff.
    pub(crate) fn stage(&mut self, observed: &Self, changes: Vec<DraftTransition>) {
        self.pending = Some(DraftBatch {
            revision: observed.revision,
            draft: observed.draft.clone(),
            changes,
        });
    }

    pub(crate) fn pending(&self) -> Option<&DraftBatch> {
        self.pending.as_ref()
    }

    /// Called only after all pending changes have durable inbox rows.
    pub(crate) fn commit_pending(&mut self) {
        if let Some(batch) = self.pending.take() {
            self.revision = batch.revision;
            self.draft = batch.draft;
        }
    }

    /// Absorb a snapshot written as `(id, body)` and report the
    /// revisions it produced. The lifecycle's tests care about which
    /// changes a horizon reports and when, not about github's rows.
    #[cfg(test)]
    pub(crate) fn absorb_snapshot(&mut self, review: u64, rows: &[(u64, &str)]) -> Vec<u64> {
        let changes = self.absorb(
            review,
            rows.iter()
                .map(|(id, body)| DraftComment {
                    id: *id,
                    path: "f.rs".into(),
                    body: (*body).into(),
                    updated_at: "T".into(),
                })
                .collect(),
        );
        changes.into_iter().map(|t| t.revision).collect()
    }

    /// A confirmed absence: there is no pending review on this PR.
    ///
    /// Reported as ONE lifecycle change, not as every comment being
    /// deleted — submitting the review ends the pending state too, and
    /// announcing a submitted review as N deletions would be a lie
    /// about what happened (codex 6a69ca7).
    fn absorb_absence(&mut self) -> Vec<DraftTransition> {
        match std::mem::replace(&mut self.draft, Draft::Absent) {
            // Nothing has gone: master has not drafted yet, and
            // reporting a loss would announce the deletion of comments
            // it is about to write.
            Draft::Unknown | Draft::Absent => Vec::new(),
            Draft::Present { .. } => vec![self.next(None, DraftChange::Gone)],
        }
    }

    /// Absorb ONE COMPLETE snapshot of the pending review `review`.
    ///
    /// The snapshot must be a whole successful read. Every remembered
    /// id it omits is reported as a deletion, so half a page would
    /// present as the operator deleting the rest — the reader raises
    /// an error rather than returning what it managed to fetch.
    pub(super) fn absorb(
        &mut self,
        review: u64,
        snapshot: Vec<DraftComment>,
    ) -> Vec<DraftTransition> {
        let fresh: BTreeMap<u64, Observed> = snapshot.iter().map(|c| (c.id, c.into())).collect();
        let previous = std::mem::replace(
            &mut self.draft,
            Draft::Present {
                review,
                seen: fresh.clone(),
            },
        );

        let seen = match previous {
            // The first draft this watch ever sees is the draft as it
            // already stood, not news.
            Draft::Unknown => return Vec::new(),
            // A draft where there was none, or a different one from
            // the one that was here: the old draft going is its own
            // change, and everything in the new one is new.
            Draft::Absent => BTreeMap::new(),
            Draft::Present { review: was, seen } if was == review => seen,
            Draft::Present { .. } => {
                let mut out = vec![self.next(None, DraftChange::Gone)];
                out.extend(self.added(&snapshot));
                return out;
            }
        };

        let mut changes = Vec::new();
        for c in &snapshot {
            let change = match seen.get(&c.id) {
                None => DraftChange::Added,
                // A changed body is a change whatever the timestamp
                // says, and a moved timestamp is a change even when
                // the body came back to what it was: both halves are
                // evidence, and neither is the key.
                Some(prev) if prev.body != c.body || prev.updated_at != c.updated_at => {
                    DraftChange::Edited
                }
                Some(_) => continue,
            };
            changes.push((c.id, c.path.clone(), change));
        }
        for (id, prev) in &seen {
            if !fresh.contains_key(id) {
                changes.push((*id, prev.path.clone(), DraftChange::Removed));
            }
        }

        changes
            .into_iter()
            .map(|(comment, path, change)| self.next(Some((comment, path)), change))
            .collect()
    }

    fn added(&mut self, snapshot: &[DraftComment]) -> Vec<DraftTransition> {
        snapshot
            .iter()
            .map(|c| self.next(Some((c.id, c.path.clone())), DraftChange::Added))
            .collect::<Vec<_>>()
    }

    /// Every change this watch detects takes the next revision,
    /// because github offers no counter that can play the part.
    fn next(&mut self, about: Option<(u64, String)>, change: DraftChange) -> DraftTransition {
        self.revision += 1;
        let (comment, path) = match about {
            Some((c, p)) => (Some(c), Some(p)),
            None => (None, None),
        };
        DraftTransition {
            comment,
            path,
            change,
            revision: self.revision,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ops::Not;

    use crate::cli::github::ci::tests::Routes;

    fn row(id: u64, path: &str, body: &str, updated_at: &str) -> serde_json::Value {
        serde_json::json!({"id": id, "path": path, "body": body, "updated_at": updated_at})
    }

    async fn read_through(
        routes: Routes,
        pr: u32,
        review: u64,
    ) -> anyhow::Result<Vec<DraftComment>> {
        GhDraft {
            repo: "o/r".into(),
            session: std::sync::Arc::new(GithubSession::for_tests(routes, || {
                Box::pin(async { Ok("t".to_string()) })
            })),
        }
        .read(pr, review)
        .await
    }

    #[tokio::test]
    async fn the_reader_asks_the_pending_review_for_its_own_comments() {
        let routes = Routes::new(vec![(
            "/comments",
            serde_json::json!([row(7, "a.rs", "hi", "T1")]),
        )]);
        let got = read_through(routes.clone(), 42, 5297636149).await.unwrap();
        assert_eq!(
            routes.asked(),
            [
                "https://api.github.com/repos/o/r/pulls/42/reviews/5297636149/comments?per_page=100&page=1"
            ],
            "the draft is only readable at the review's own comments url"
        );
        assert_eq!(got, vec![c2(7, "a.rs", "hi", "T1")]);
    }

    #[tokio::test]
    async fn a_row_missing_any_field_fails_the_whole_snapshot() {
        for missing in ["id", "path", "body", "updated_at"] {
            let mut bad = row(7, "a.rs", "hi", "T1");
            bad.as_object_mut().unwrap().remove(missing);
            let routes = Routes::new(vec![(
                "/comments",
                serde_json::json!([row(1, "b.rs", "ok", "T1"), bad]),
            )]);
            let err = read_through(routes, 42, 1).await.unwrap_err().to_string();
            assert!(
                err.contains(missing),
                "a row with no `{missing}` must fail the snapshot, not enter it half-read: {err}"
            );
        }
    }

    #[tokio::test]
    async fn a_failed_read_is_an_error_and_never_an_empty_draft() {
        let err = read_through(Routes::failing(503), 42, 1).await.unwrap_err();
        assert!(err.to_string().contains("503"), "{err}");
    }

    fn c2(id: u64, path: &str, body: &str, updated_at: &str) -> DraftComment {
        DraftComment {
            id,
            path: path.into(),
            body: body.into(),
            updated_at: updated_at.into(),
        }
    }

    /// Answers requests IN ORDER rather than by url, so a test can
    /// make the collection move between two pages of one read.
    #[derive(Clone)]
    struct Pages(std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<serde_json::Value>>>);

    impl Pages {
        fn new(pages: Vec<Vec<serde_json::Value>>) -> Self {
            Self(std::sync::Arc::new(std::sync::Mutex::new(
                pages.into_iter().map(serde_json::Value::from).collect(),
            )))
        }
        fn left(&self) -> usize {
            self.0.lock().unwrap().len()
        }
    }

    impl HttpSend for Pages {
        async fn send(
            &self,
            _url: &str,
            _bearer: &str,
            _etag: Option<&str>,
        ) -> anyhow::Result<crate::cli::github::session::MiniResponse> {
            let body = self
                .0
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| serde_json::json!([]));
            Ok(crate::cli::github::session::MiniResponse {
                status: 200,
                headers: Default::default(),
                body: body.to_string(),
            })
        }
    }

    fn ids(from: u64, to: u64) -> Vec<serde_json::Value> {
        (from..=to).map(|i| row(i, "a.rs", "b", "T1")).collect()
    }

    async fn read_pages(pages: Pages) -> anyhow::Result<Vec<DraftComment>> {
        GhDraft {
            repo: "o/r".into(),
            session: std::sync::Arc::new(GithubSession::for_tests(pages, || {
                Box::pin(async { Ok("t".to_string()) })
            })),
        }
        .read(42, 1)
        .await
    }

    #[tokio::test]
    async fn a_comment_deleted_between_pages_never_removes_the_wrong_one() {
        // codex 97355af: ids 1..=101 exist; page 1 returns 1..=100,
        // then the operator deletes 1, so the shifted remainder fits
        // in one page and page 2 comes back empty. A reader that
        // called that a snapshot would report 101 removed — the one
        // comment that did NOT go.
        let pages = Pages::new(vec![
            ids(1, 100),
            vec![],
            ids(2, 101),
            vec![],
            ids(2, 101),
            vec![],
        ]);
        let snapshot = read_pages(pages).await.unwrap();
        assert_eq!(
            snapshot.iter().map(|c| c.id).collect::<Vec<_>>(),
            (2..=101).collect::<Vec<_>>(),
            "the settled read is the one that agrees with itself"
        );

        let mut h = DraftHorizon::default();
        h.absorb(1, (1..=101).map(|i| c2(i, "a.rs", "b", "T1")).collect());
        assert_eq!(
            kinds(&h.absorb(1, snapshot)),
            [(Some(1), DraftChange::Removed)],
            "the comment that went is the one reported"
        );
    }

    #[tokio::test]
    async fn a_draft_that_never_settles_is_refused_not_guessed_at() {
        let pages = Pages::new(vec![
            ids(1, 100),
            vec![],
            ids(2, 101),
            vec![],
            ids(3, 102),
            vec![],
            ids(4, 103),
            vec![],
        ]);
        let err = read_pages(pages).await.unwrap_err().to_string();
        assert!(err.contains("changed under"), "{err}");
    }

    struct DeadDraft;
    impl DraftReader for DeadDraft {
        async fn pending_review(&self, _: u32) -> anyhow::Result<Option<u64>> {
            Ok(Some(1))
        }
        async fn read(&self, _: u32, _: u64) -> anyhow::Result<Vec<DraftComment>> {
            anyhow::bail!("github is down")
        }
    }

    struct OneDraft(Option<u64>, Vec<DraftComment>);
    impl DraftReader for OneDraft {
        async fn pending_review(&self, _: u32) -> anyhow::Result<Option<u64>> {
            Ok(self.0)
        }
        async fn read(&self, _: u32, _: u64) -> anyhow::Result<Vec<DraftComment>> {
            Ok(self.1.clone())
        }
    }

    #[tokio::test]
    async fn a_read_it_could_not_trust_leaves_the_horizon_where_it_was() {
        let mut h = DraftHorizon::default();
        observe(&OneDraft(Some(1), vec![c(1, "a", "T1")]), 42, &mut h)
            .await
            .unwrap();

        let before = h.clone();
        assert!(observe(&DeadDraft, 42, &mut h).await.is_err());
        assert_eq!(
            h, before,
            "a failed read is not evidence the draft is empty"
        );

        assert_eq!(
            kinds(
                &observe(&OneDraft(Some(1), vec![c(1, "b", "T2")]), 42, &mut h)
                    .await
                    .unwrap()
            ),
            [(Some(1), DraftChange::Edited)],
            "and the change it missed is still there to find"
        );
    }

    #[tokio::test]
    async fn a_draft_that_was_never_there_is_not_a_loss() {
        let mut h = DraftHorizon::default();
        assert!(
            observe(&OneDraft(None, vec![]), 42, &mut h)
                .await
                .unwrap()
                .is_empty(),
            "master has not drafted; announcing a loss would announce comments it is about to write"
        );
        assert!(
            observe(&OneDraft(Some(9), vec![c(1, "a", "T1")]), 42, &mut h)
                .await
                .unwrap()
                .is_empty()
                .not(),
            "but a draft appearing where the observer CONFIRMED none is new information"
        );
    }

    #[tokio::test]
    async fn a_draft_that_goes_away_is_one_change_reported_once() {
        let mut h = DraftHorizon::default();
        observe(
            &OneDraft(Some(9), vec![c(1, "a", "T1"), c(2, "b", "T1")]),
            42,
            &mut h,
        )
        .await
        .unwrap();

        let gone = observe(&OneDraft(None, vec![]), 42, &mut h).await.unwrap();
        assert_eq!(
            kinds(&gone),
            [(None, DraftChange::Gone)],
            "submitting the review ends the pending state too, so this is not two deletions"
        );
        assert_eq!(gone[0].revision, 1);

        assert!(
            observe(&OneDraft(None, vec![]), 42, &mut h)
                .await
                .unwrap()
                .is_empty(),
            "and it is spent: a draft that is still gone is not news again"
        );
    }

    #[tokio::test]
    async fn a_draft_replaced_by_another_is_the_old_one_going_and_the_new_one_arriving() {
        let mut h = DraftHorizon::default();
        observe(&OneDraft(Some(9), vec![c(1, "a", "T1")]), 42, &mut h)
            .await
            .unwrap();
        assert_eq!(
            kinds(
                &observe(&OneDraft(Some(10), vec![c(7, "fresh", "T2")]), 42, &mut h)
                    .await
                    .unwrap()
            ),
            [(None, DraftChange::Gone), (Some(7), DraftChange::Added)],
            "a discarded-and-restarted review is not an edit of the dead one's comments"
        );
    }

    #[tokio::test]
    async fn a_draft_inside_one_page_is_already_a_snapshot() {
        let pages = Pages::new(vec![ids(1, 3)]);
        let p = pages.clone();
        let got = read_pages(pages).await.unwrap();
        assert_eq!(got.len(), 3);
        assert_eq!(p.left(), 0, "one response, and no re-read to confirm it");
    }

    fn c(id: u64, body: &str, updated_at: &str) -> DraftComment {
        DraftComment {
            id,
            path: "PROBE.md".into(),
            body: body.into(),
            updated_at: updated_at.into(),
        }
    }

    fn kinds(ts: &[DraftTransition]) -> Vec<(Option<u64>, DraftChange)> {
        ts.iter().map(|t| (t.comment, t.change)).collect()
    }

    #[test]
    fn the_first_snapshot_is_a_baseline_and_says_nothing() {
        let mut h = DraftHorizon::default();
        assert!(
            h.absorb(1, vec![c(1, "a", "T1"), c(2, "b", "T1")])
                .is_empty(),
            "a draft that already had comments is not news"
        );
        assert!(
            h.absorb(1, vec![c(1, "a", "T1"), c(2, "b", "T1")])
                .is_empty(),
            "and an unchanged snapshot stays quiet"
        );
    }

    #[test]
    fn a_body_that_changed_inside_one_second_is_still_a_change() {
        // The probe could not produce this case — its two edits were
        // 119s and 41s apart — but `updated_at` is whole-second, so a
        // reader keyed on it would drop a change it SAW (codex).
        let mut h = DraftHorizon::default();
        h.absorb(1, vec![c(1, "a", "T1")]);
        assert_eq!(
            kinds(&h.absorb(1, vec![c(1, "EDITED", "T1")])),
            [(Some(1), DraftChange::Edited)],
            "same id, same timestamp, different body"
        );
    }

    #[test]
    fn an_edit_back_to_the_original_body_presents_again() {
        let mut h = DraftHorizon::default();
        h.absorb(1, vec![c(1, "a", "T1")]);
        assert_eq!(
            kinds(&h.absorb(1, vec![c(1, "b", "T2")])),
            [(Some(1), DraftChange::Edited)]
        );
        assert_eq!(
            kinds(&h.absorb(1, vec![c(1, "a", "T3")])),
            [(Some(1), DraftChange::Edited)],
            "A -> B -> A is two changes, not a return to a seen state"
        );
    }

    #[test]
    fn a_timestamp_that_moved_with_an_unchanged_body_is_a_change() {
        let mut h = DraftHorizon::default();
        h.absorb(1, vec![c(1, "a", "T1")]);
        assert_eq!(
            kinds(&h.absorb(1, vec![c(1, "a", "T2")])),
            [(Some(1), DraftChange::Edited)],
            "the operator touched it; the body landing back is not evidence it did not happen"
        );
    }

    #[test]
    fn an_added_and_a_removed_comment_each_present() {
        let mut h = DraftHorizon::default();
        h.absorb(1, vec![c(1, "a", "T1"), c(2, "b", "T1")]);
        assert_eq!(
            kinds(&h.absorb(1, vec![c(1, "a", "T1"), c(3, "new", "T2")])),
            [
                (Some(3), DraftChange::Added),
                (Some(2), DraftChange::Removed)
            ],
        );
    }

    #[test]
    fn every_detected_change_takes_the_next_revision() {
        let mut h = DraftHorizon::default();
        h.absorb(1, vec![c(1, "a", "T1"), c(2, "b", "T1")]);
        let first = h.absorb(1, vec![c(1, "a2", "T2"), c(2, "b2", "T2")]);
        assert_eq!(
            first.iter().map(|t| t.revision).collect::<Vec<_>>(),
            [1, 2],
            "the revision counts changes, not ticks"
        );
        let second = h.absorb(1, vec![c(1, "a3", "T3"), c(2, "b2", "T2")]);
        assert_eq!(second.iter().map(|t| t.revision).collect::<Vec<_>>(), [3]);
    }

    #[test]
    fn a_horizon_survives_a_round_trip_through_its_file() {
        let mut h = DraftHorizon::default();
        h.absorb(1, vec![c(1, "a", "T1")]);
        h.absorb(1, vec![c(1, "b", "T2")]);
        let back: DraftHorizon = serde_json::from_str(&serde_json::to_string(&h).unwrap()).unwrap();
        assert_eq!(back, h);
        let mut back = back;
        assert!(
            back.absorb(1, vec![c(1, "b", "T2")]).is_empty(),
            "a restored horizon does not re-present what it already said"
        );
        assert_eq!(
            back.absorb(1, vec![c(1, "c", "T3")])[0].revision,
            2,
            "and the revision continues rather than restarting"
        );
    }
}
