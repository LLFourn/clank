//! One PR head's CI result (a-red-build-is-a-wake).
//!
//! A PR is not green because a check suite is: a suite summarizes the
//! runs of ONE GitHub App, and a repo's checks routinely come from
//! several, plus the older commit-status API. So no single suite,
//! run, or webhook delivery is evidence about the PR — the result is
//! a reduction over every check that reports on the head commit.
//!
//! Everything here is pure. Readers (the poll's REST calls, and the
//! relay's reconciliation after a webhook) normalize whatever they
//! fetched into [`CheckReport`]s and reduce them the same way, so a
//! conclusion cannot depend on which transport observed it.

use clank_core::agent_config::GithubEventKind;
use clank_core::wait::WaitItem;

use super::feed::Classified;
use super::selection::{Event, Selection};
use super::session::{GithubSession, HttpSend, PER_PAGE, read_every_page};

/// What one check says about a commit.
///
/// Only the conclusion: WHICH runs report on the head is the reader's
/// question, answered by asking GitHub for that commit's checks, and
/// mixing the two here is how a result that belongs to the head gets
/// dropped for looking superseded (codex 008eb13).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckState {
    /// Queued, running, or reported no conclusion we recognize —
    /// anything not yet decided. Never wakes on its own.
    Pending,
    Passed,
    Failed,
}

/// One check's report on a commit, normalized from either of GitHub's
/// two surfaces: the Checks API's runs and the older commit statuses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckReport {
    pub name: String,
    pub state: CheckState,
}

/// A PR head's CI result — what the two wake kinds are derived from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CiRollup {
    /// Nothing reports on this commit. Distinct from
    /// [`Pending`](Self::Pending): a repo with no CI is not a build
    /// that has not finished, and neither wakes.
    NoChecks,
    Pending,
    Passed,
    /// At least one check reached a terminal state that is not a
    /// success. `failing` names them, in the order they were read, so
    /// the wake can say what broke.
    Failed {
        failing: Vec<String>,
    },
}

/// Reduce every check reporting on one commit to that commit's result.
///
/// Failure dominates: a red check with another still running is
/// already a build you can act on, and waiting for the stragglers only
/// delays the wake. Absent any failure, one outstanding check makes
/// the whole result pending.
pub fn reduce(reports: &[CheckReport]) -> CiRollup {
    let mut counted = 0usize;
    let mut pending = false;
    let mut failing: Vec<String> = Vec::new();
    for report in reports {
        match report.state {
            CheckState::Pending => pending = true,
            CheckState::Failed => failing.push(report.name.clone()),
            CheckState::Passed => {}
        }
        counted += 1;
    }
    if !failing.is_empty() {
        return CiRollup::Failed { failing };
    }
    if counted == 0 {
        return CiRollup::NoChecks;
    }
    if pending {
        return CiRollup::Pending;
    }
    CiRollup::Passed
}

/// A check run's state, from its `status` and `conclusion`.
///
/// `conclusion` is only meaningful once `status` is `completed`. The
/// mapping is deliberate about the non-success terminal states: a run
/// that was cancelled, timed out, was marked `stale` for taking too
/// long, or is asking for a human did not pass, and a wake that only
/// fired on `failure` would call such a PR green. `neutral` and
/// `skipped` did not fail and do not block. Anything unrecognized is
/// pending — never wake on a state this binary does not understand.
pub fn check_run_state(status: &str, conclusion: Option<&str>) -> CheckState {
    if status != "completed" {
        return CheckState::Pending;
    }
    match conclusion {
        Some("success" | "neutral" | "skipped") => CheckState::Passed,
        Some("failure" | "timed_out" | "action_required" | "cancelled" | "stale") => {
            CheckState::Failed
        }
        _ => CheckState::Pending,
    }
}

/// A commit status's state (the older API: `pending`, `success`,
/// `failure`, `error`).
pub fn commit_status_state(state: &str) -> CheckState {
    match state {
        "success" => CheckState::Passed,
        "failure" | "error" => CheckState::Failed,
        _ => CheckState::Pending,
    }
}

/// Normalize the `check_runs` of
/// `GET /repos/{owner}/{repo}/commits/{ref}/check-runs`, across every
/// page.
///
/// A run this cannot read is PENDING, not absent. Skipping it would
/// be the one place this module breaks its own rule — never wake on a
/// state it does not understand — and it would break it on the side
/// that announces green (ruthless de2b1f7).
pub fn reports_from_check_runs(runs: &[serde_json::Value]) -> Vec<CheckReport> {
    runs.iter()
        .map(|run| CheckReport {
            name: field(run, "name")
                .unwrap_or("(unreadable check)")
                .to_string(),
            state: match field(run, "status") {
                Some(status) => check_run_state(status, field(run, "conclusion")),
                None => CheckState::Pending,
            },
        })
        .collect()
}

fn field<'a>(value: &'a serde_json::Value, name: &str) -> Option<&'a str> {
    value.get(name)?.as_str()
}

/// Normalize the `statuses` of
/// `GET /repos/{owner}/{repo}/commits/{ref}/status` — the latest
/// status per context, across every page.
///
/// The response's own combined `state` is NOT read: it is GitHub's
/// reduction of these same contexts under GitHub's policy, and this
/// module has its own (see [`reduce`]). Reading both would mean two
/// answers that can disagree.
pub fn reports_from_commit_statuses(statuses: &[serde_json::Value]) -> Vec<CheckReport> {
    statuses
        .iter()
        .map(|status| CheckReport {
            name: field(status, "context")
                .unwrap_or("(unreadable status)")
                .to_string(),
            // Unreadable is pending, for the same reason as a run.
            state: field(status, "state").map_or(CheckState::Pending, commit_status_state),
        })
        .collect()
}

/// An open PR and the commit its checks report on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PrHead {
    pub(crate) number: u64,
    pub(crate) head: String,
}

/// The open PRs of `repo` with their head commits. This is also what
/// bounds the work: only open PRs are read for checks.
///
/// A PR named in a watch's `prs` that is not open simply is not here.
/// A closed PR's build is not news, and asking for it by number would
/// cost a request per tick to learn that every time.
pub(crate) async fn open_prs<S: HttpSend>(
    session: &GithubSession<S>,
    repo: &str,
) -> anyhow::Result<Vec<PrHead>> {
    let pulls = read_every_page(
        session,
        |page| {
            format!(
                "https://api.github.com/repos/{repo}/pulls?state=open&per_page={PER_PAGE}&page={page}"
            )
        },
        None,
    )
    .await?;
    Ok(heads_from_pulls(&pulls))
}

/// Every check reporting on `head`, from BOTH surfaces — the Checks
/// API's runs and the older commit statuses — each read to the last
/// page. A PR's result is the union: either surface alone can be the
/// whole story.
pub(crate) async fn checks_for<S: HttpSend>(
    session: &GithubSession<S>,
    repo: &str,
    head: &str,
) -> anyhow::Result<Vec<CheckReport>> {
    let runs = read_every_page(
        session,
        |page| {
            format!(
                "https://api.github.com/repos/{repo}/commits/{head}/check-runs?per_page={PER_PAGE}&page={page}"
            )
        },
        Some("check_runs"),
    )
    .await?;
    let statuses = read_every_page(
        session,
        |page| {
            format!(
                "https://api.github.com/repos/{repo}/commits/{head}/status?per_page={PER_PAGE}&page={page}"
            )
        },
        Some("statuses"),
    )
    .await?;
    let mut reports = reports_from_check_runs(&runs);
    reports.extend(reports_from_commit_statuses(&statuses));
    Ok(reports)
}

/// Pure half of [`open_prs`]. A PR with no number or no head sha is
/// skipped: a manufactured one would key a wake to the wrong build.
pub(crate) fn heads_from_pulls(pulls: &[serde_json::Value]) -> Vec<PrHead> {
    pulls
        .iter()
        .filter_map(|pr| {
            Some(PrHead {
                number: pr.get("number")?.as_u64()?,
                head: pr.get("head")?.get("sha")?.as_str()?.to_string(),
            })
        })
        .collect()
}

/// Which wake a result is, if it is one. Pending and NoChecks are not
/// news: nothing has concluded.
pub(crate) fn kind_of(rollup: &CiRollup) -> Option<GithubEventKind> {
    match rollup {
        CiRollup::Passed => Some(GithubEventKind::CiPassed),
        CiRollup::Failed { .. } => Some(GithubEventKind::CiFailed),
        CiRollup::Pending | CiRollup::NoChecks => None,
    }
}

/// Whether an action key is one of this module's.
///
/// A WAL row keyed like this is evidence about the CHECKS, never
/// about the feed — telling them apart is what stops a checks-only
/// log from looking like a feed that has baselined (codex 7c132ec).
pub(crate) fn is_ci_key(key: &str) -> bool {
    key.starts_with("ci_passed#") || key.starts_with("ci_failed#")
}

/// The dedup identity of a conclusion: which PR, which head, which
/// way it went.
///
/// Minted by whichever path observed it — the poll's read or the
/// webhook-triggered re-read — so the coordinator collapses the two
/// into one wake with no machinery of its own. A rollup read carries
/// no feed id, so this key is the ONLY identity a CI row has: without
/// it the log groups rows by a hash of their content and two runs of
/// one build become indistinguishable.
///
/// Its consequence, accepted deliberately: an unchanged head that
/// concludes the same way twice wakes once, even with an opposite
/// result between them (a re-run going red, green, red is two wakes,
/// not three).
pub(crate) fn action_key(kind: GithubEventKind, pr: u64, head: &str) -> String {
    format!("{}#{pr}@{head}", crate::cli::github::feed::kind_str(kind))
}

/// Every in-scope PR's current result. `prs` empty means every open
/// PR the watch can see.
///
/// The cost is one request for the PR list plus two per PR in scope,
/// which is why the scope exists: it is a filter on WHAT IS FETCHED,
/// not on what is reported afterwards.
pub(crate) async fn read_conclusions<S: HttpSend>(
    session: &GithubSession<S>,
    repo: &str,
    prs: &[u64],
) -> anyhow::Result<Vec<(PrHead, CiRollup)>> {
    let mut out = Vec::new();
    for pr in open_prs(session, repo).await? {
        if !prs.is_empty() && !prs.contains(&pr.number) {
            continue;
        }
        let rollup = reduce(&checks_for(session, repo, &pr.head).await?);
        out.push((pr, rollup));
    }
    Ok(out)
}

/// How many failing checks a wake names before it counts the rest.
const NAMED_FAILURES: usize = 3;

/// The wake for one PR's conclusion: what happened, and where to
/// look. No actor — a conclusion is not somebody's action, which is
/// also why the own-action filter has nothing to say about it.
pub(crate) fn item(repo: &str, pr: &PrHead, rollup: &CiRollup) -> Option<WaitItem> {
    let kind = kind_of(rollup)?;
    let title = match rollup {
        CiRollup::Failed { failing } => {
            let named: Vec<&str> = failing
                .iter()
                .take(NAMED_FAILURES)
                .map(String::as_str)
                .collect();
            let rest = failing.len() - named.len();
            let more = if rest > 0 {
                format!(" (+{rest} more)")
            } else {
                String::new()
            };
            format!("CI failed: {}{more}", named.join(", "))
        }
        _ => "CI passed".to_string(),
    };
    Some(WaitItem::GithubEvent {
        repo: repo.to_string(),
        event: crate::cli::github::feed::kind_str(kind).to_string(),
        detail: None,
        number: Some(pr.number),
        title: Some(title),
        actor: None,
        url: Some(format!(
            "https://github.com/{repo}/pull/{}/checks",
            pr.number
        )),
        instructions: None,
        content: Some(clank_core::wait::ContentRef::Pr { number: pr.number }),
    })
}

/// The wake for a conclusion, ready for the emission gate — or
/// `None` when the result is not news.
///
/// No feed id: a CI row is identified by its action key alone, which
/// is why [`action_key`] is not optional here the way a payload's can
/// be. No `event_at` either — a rollup is read, not delivered, so the
/// timeline falls back to ingest time.
pub(crate) fn classified(repo: &str, pr: &PrHead, rollup: &CiRollup) -> Option<Classified> {
    let kind = kind_of(rollup)?;
    Some(Classified {
        item: item(repo, pr, rollup)?,
        key: Some(action_key(kind, pr.number, &pr.head)),
        feed_id: None,
        event_at: None,
    })
}

/// One PR's conclusion, ready for the gate.
pub(crate) struct CiWake {
    pub(crate) pr: u64,
    pub(crate) head: String,
    pub(crate) kind: GithubEventKind,
    pub(crate) action: Classified,
}

/// What a CI watch remembers: whether it has ever read successfully,
/// and what it has already said about each PR's current head.
///
/// Its OWN state, in its own file. The feed's cache is a cursor over
/// a stream — an etag, a server floor, a recency window of ids — and
/// every attempt to express a state read in those terms produced a
/// bug: one shared "this log has records" flag made one half's
/// history establish the other, and a recency window over events
/// evicted conclusions that a state read re-observes forever (codex
/// 7c132ec, ae48e92). Nothing here is shared with that.
///
/// Losing the file is the quiet failure: the watch re-establishes and
/// says nothing, rather than announcing builds that were already the
/// way they are.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct CiHorizon {
    /// Whether a read has ever succeeded. Until one has, what the
    /// checks say is the state of the world, not news about it.
    #[serde(default)]
    established: bool,
    /// Per PR, what has been said about its CURRENT head.
    #[serde(default)]
    said: std::collections::BTreeMap<u64, PrSaid>,
}

/// The conclusions already announced for one PR's head. BOTH of them
/// when both have happened: a re-run going red, green, red must say
/// "red" once, and remembering only the latest makes the second red
/// look new.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct PrSaid {
    head: String,
    kinds: std::collections::BTreeSet<String>,
}

impl CiHorizon {
    /// Read the horizon, or start an empty one. A missing or
    /// unreadable file is not an error: re-establishing is silent.
    pub(crate) fn load(path: Option<&std::path::Path>) -> Self {
        path.and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    /// Persist it. Best effort, atomically: a failed write costs the
    /// next arm a silent re-establish, never a wrong wake.
    pub(crate) fn save(&self, path: Option<&std::path::Path>) {
        let Some(path) = path else { return };
        let Ok(bytes) = serde_json::to_vec(self) else {
            return;
        };
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, bytes).is_ok() {
            let _ = std::fs::rename(&tmp, path);
        }
    }

    /// Take a read's wakes and return the ones that are NEWS,
    /// updating what is remembered.
    ///
    /// `in_scope` is every PR the read covered: one that is no longer
    /// there takes its memory with it, so retention follows the PRs
    /// being watched rather than the volume of activity around them.
    /// The first read of a watch establishes it and announces
    /// nothing.
    pub(crate) fn absorb(
        &mut self,
        wakes: Vec<CiWake>,
        in_scope: &std::collections::BTreeSet<u64>,
    ) -> Vec<Classified> {
        self.said.retain(|pr, _| in_scope.contains(pr));
        let mut news = Vec::new();
        for w in wakes {
            let said = self.said.entry(w.pr).or_default();
            if said.head != w.head {
                // A new head is a new build: nothing said about the
                // old one applies to it.
                said.head = w.head;
                said.kinds.clear();
            }
            let fresh = said
                .kinds
                .insert(crate::cli::github::feed::kind_str(w.kind).to_string());
            if fresh && self.established {
                news.push(w.action);
            }
        }
        self.established = true;
        news
    }

    #[cfg(test)]
    pub(crate) fn is_established(&self) -> bool {
        self.established
    }
}

/// The wakes a read produces for `sel` — the whole mapping from
/// results to gate-ready actions, filter included, so a watch listing
/// only `ci_failed` is never woken by green.
///
/// The SELECTION, not a list of kinds. A clause names an event AND
/// the objects it is about, so `ci_passed` on one PR beside
/// `ci_failed` on another must not deliver either one crossed — kind
/// membership alone said yes to both (codex bcda3c8). A conclusion is
/// nobody's action, so the actor filter has nothing to say about it,
/// and it names a PR by construction, so it can never be the
/// unplaceable case.
pub(crate) fn wakes(repo: &str, sel: &Selection, results: &[(PrHead, CiRollup)]) -> Vec<CiWake> {
    results
        .iter()
        .filter_map(|(pr, rollup)| {
            let kind = kind_of(rollup)?;
            if !sel
                .admits(&Event {
                    kind,
                    detail: None,
                    number: Some(pr.number),
                    branch: None,
                    is_own: false,
                })
                .admitted()
            {
                return None;
            }
            Some(CiWake {
                pr: pr.number,
                head: pr.head.clone(),
                kind,
                action: classified(repo, pr, rollup)?,
            })
        })
        .collect()
}

/// Reads a repo's CI conclusions — injected like the events fetcher,
/// so the loop's CI horizon is testable without a network.
pub(crate) trait CiReader {
    fn read(
        &self,
    ) -> impl Future<
        Output = anyhow::Result<
            Vec<(
                crate::cli::github::ci::PrHead,
                crate::cli::github::ci::CiRollup,
            )>,
        >,
    > + Send;
}

/// Production reader: the checks endpoints over the SHARED session.
pub(super) struct GhCi<S: HttpSend> {
    pub(super) repo: String,
    pub(super) prs: Vec<u64>,
    pub(super) session: std::sync::Arc<GithubSession<S>>,
}

impl<S: HttpSend> CiReader for GhCi<S> {
    async fn read(
        &self,
    ) -> anyhow::Result<
        Vec<(
            crate::cli::github::ci::PrHead,
            crate::cli::github::ci::CiRollup,
        )>,
    > {
        crate::cli::github::ci::read_conclusions(&self.session, &self.repo, &self.prs).await
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use std::sync::Mutex;

    use crate::cli::github::ci;
    use crate::cli::github::session::{MAX_COLLECTION_PAGES, MiniResponse};

    /// Scripted HTTP that answers by URL and records what was asked,
    /// so the reader's own urls are under test rather than assumed.
    #[derive(Clone)]
    pub(in crate::cli::github) struct Routes {
        bodies: std::sync::Arc<Vec<(&'static str, String)>>,
        asked: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        status: u16,
    }
    impl Routes {
        pub(in crate::cli::github) fn new(bodies: Vec<(&'static str, serde_json::Value)>) -> Self {
            Self {
                bodies: std::sync::Arc::new(
                    bodies
                        .into_iter()
                        .map(|(frag, body)| (frag, body.to_string()))
                        .collect(),
                ),
                asked: Default::default(),
                status: 200,
            }
        }
        pub(in crate::cli::github) fn failing(status: u16) -> Self {
            Self {
                bodies: Default::default(),
                asked: Default::default(),
                status,
            }
        }
        pub(in crate::cli::github) fn asked(&self) -> Vec<String> {
            self.asked.lock().unwrap().clone()
        }
    }
    impl HttpSend for Routes {
        async fn send(
            &self,
            url: &str,
            _bearer: &str,
            _etag: Option<&str>,
        ) -> anyhow::Result<MiniResponse> {
            self.asked.lock().unwrap().push(url.to_string());
            let body = self
                .bodies
                .iter()
                .find(|(frag, _)| url.contains(frag))
                .map(|(_, body)| body.clone())
                .unwrap_or_else(|| "null".to_string());
            Ok(MiniResponse {
                status: self.status,
                headers: Default::default(),
                body,
            })
        }
    }

    fn token() -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<String>> + Send>>
    {
        Box::pin(async { Ok("tok".to_string()) })
    }

    #[tokio::test]
    async fn open_prs_reads_each_number_and_its_head() {
        let routes = Routes::new(vec![(
            "/pulls",
            serde_json::json!([
                { "number": 7, "head": { "sha": "aaaa111" } },
                { "number": 9, "head": { "sha": "bbbb222" } },
                { "number": 11 },
            ]),
        )]);
        let session = GithubSession::for_tests(routes.clone(), token);
        let heads = open_prs(&session, "o/r").await.unwrap();
        assert_eq!(
            heads,
            vec![
                PrHead {
                    number: 7,
                    head: "aaaa111".into()
                },
                PrHead {
                    number: 9,
                    head: "bbbb222".into()
                },
            ],
            "a PR with no head sha is skipped, never guessed at",
        );
        assert!(
            routes.asked().iter().any(|u| u.contains("state=open")),
            "only open PRs have a build worth waking for",
        );
    }

    #[tokio::test]
    async fn checks_for_asks_both_surfaces_and_unions_them() {
        let routes = Routes::new(vec![
            (
                "/check-runs",
                serde_json::json!({
                    "check_runs": [
                        { "name": "test", "status": "completed", "conclusion": "success" },
                    ]
                }),
            ),
            (
                "/status",
                serde_json::json!({
                    "statuses": [ { "context": "ci/legacy", "state": "failure" } ]
                }),
            ),
        ]);
        let session = GithubSession::for_tests(routes.clone(), token);
        let reports = checks_for(&session, "o/r", "aaaa111").await.unwrap();
        assert_eq!(
            reduce(&reports),
            CiRollup::Failed {
                failing: vec!["ci/legacy".into()]
            },
            "a red commit status makes the PR red even when every check run passed",
        );
        let asked = routes.asked();
        assert!(
            asked
                .iter()
                .any(|u| u.contains("/commits/aaaa111/check-runs")),
            "{asked:?}"
        );
        assert!(
            asked.iter().any(|u| u.contains("/commits/aaaa111/status")),
            "{asked:?}"
        );
    }

    #[tokio::test]
    async fn a_named_scope_reads_only_those_prs() {
        // The scope is a bound on WHAT IS FETCHED, not a filter on
        // what is reported afterwards: an out-of-scope PR must cost
        // no requests at all (codex 8b9a393).
        let bodies = || {
            vec![
                (
                    "/pulls",
                    serde_json::json!([
                        { "number": 7, "head": { "sha": "head7" } },
                        { "number": 8, "head": { "sha": "head8" } },
                    ]),
                ),
                (
                    "/commits/head7/check-runs",
                    serde_json::json!({
                        "check_runs": [
                            { "name": "test", "status": "completed", "conclusion": "failure" },
                        ]
                    }),
                ),
                (
                    "/commits/head7/status",
                    serde_json::json!({ "statuses": [] }),
                ),
                (
                    "/commits/head8/check-runs",
                    serde_json::json!({
                        "check_runs": [
                            { "name": "test", "status": "completed", "conclusion": "failure" },
                        ]
                    }),
                ),
                (
                    "/commits/head8/status",
                    serde_json::json!({ "statuses": [] }),
                ),
            ]
        };

        let narrowed = Routes::new(bodies());
        let session = GithubSession::for_tests(narrowed.clone(), token);
        let results = read_conclusions(&session, "o/r", &[7]).await.unwrap();
        assert_eq!(
            results.iter().map(|(pr, _)| pr.number).collect::<Vec<_>>(),
            vec![7],
        );
        assert!(matches!(results[0].1, CiRollup::Failed { .. }));
        let asked = narrowed.asked();
        assert!(
            asked.iter().any(|u| u.contains("head7")),
            "the PR in scope is read: {asked:?}",
        );
        assert!(
            !asked.iter().any(|u| u.contains("head8")),
            "and the one out of scope costs nothing: {asked:?}",
        );

        let wide = Routes::new(bodies());
        let session = GithubSession::for_tests(wide.clone(), token);
        let results = read_conclusions(&session, "o/r", &[]).await.unwrap();
        assert_eq!(
            results.iter().map(|(pr, _)| pr.number).collect::<Vec<_>>(),
            vec![7, 8],
            "an empty scope is every open PR",
        );
        let asked = wide.asked();
        assert!(asked.iter().any(|u| u.contains("head7")), "{asked:?}");
        assert!(asked.iter().any(|u| u.contains("head8")), "{asked:?}");
    }

    #[tokio::test]
    async fn a_refused_request_names_the_url() {
        let session = GithubSession::for_tests(Routes::failing(404), token);
        let err = open_prs(&session, "o/r").await.unwrap_err().to_string();
        assert!(err.contains("404"), "{err}");
        assert!(err.contains("/pulls"), "{err}");
    }

    #[test]
    fn only_a_conclusion_is_news() {
        let pr = PrHead {
            number: 7,
            head: "aaaa111".into(),
        };
        assert!(item("o/r", &pr, &CiRollup::Pending).is_none());
        assert!(item("o/r", &pr, &CiRollup::NoChecks).is_none());
        assert!(item("o/r", &pr, &CiRollup::Passed).is_some());
    }

    #[test]
    fn the_wake_is_addressed_to_its_pr_and_says_what_broke() {
        let pr = PrHead {
            number: 7,
            head: "aaaa111".into(),
        };
        let failing = CiRollup::Failed {
            failing: vec![
                "fmt".into(),
                "clippy".into(),
                "test".into(),
                "doc".into(),
                "audit".into(),
            ],
        };
        let WaitItem::GithubEvent {
            repo,
            event,
            number,
            title,
            actor,
            url,
            content,
            ..
        } = item("o/r", &pr, &failing).unwrap()
        else {
            panic!("a github event");
        };
        assert_eq!(repo, "o/r");
        assert_eq!(event, "ci_failed");
        assert_eq!(number, Some(7));
        assert_eq!(
            title.as_deref(),
            Some("CI failed: fmt, clippy, test (+2 more)")
        );
        assert_eq!(actor, None, "a conclusion is nobody's action");
        assert_eq!(url.as_deref(), Some("https://github.com/o/r/pull/7/checks"));
        assert_eq!(
            content,
            Some(clank_core::wait::ContentRef::Pr { number: 7 })
        );
    }

    fn full_page(make: impl Fn(usize) -> serde_json::Value) -> Vec<serde_json::Value> {
        (0..PER_PAGE).map(make).collect()
    }

    #[tokio::test]
    async fn a_pr_on_the_second_page_is_still_watched() {
        // A repo with more than a page of open PRs must not hide the
        // one being watched (codex 3707a88).
        let page1 = full_page(
            |i| serde_json::json!({ "number": i + 1, "head": { "sha": format!("head{i}") } }),
        );
        let routes = Routes::new(vec![
            (
                "/pulls?state=open&per_page=100&page=2",
                serde_json::json!([{ "number": 999, "head": { "sha": "late" } }]),
            ),
            ("/pulls", serde_json::Value::Array(page1)),
        ]);
        let session = GithubSession::for_tests(routes.clone(), token);
        let heads = open_prs(&session, "o/r").await.unwrap();
        assert_eq!(heads.len(), PER_PAGE + 1);
        assert_eq!(
            heads.last().unwrap(),
            &PrHead {
                number: 999,
                head: "late".into()
            },
        );
        assert_eq!(routes.asked().len(), 2, "a short page ends the read");
    }

    #[tokio::test]
    async fn a_failure_on_a_later_page_is_not_a_green_build() {
        // 100 green runs on page one and the failure on page two:
        // reading only the first page reports a PR green that is not.
        let green = full_page(|i| {
            serde_json::json!({
                "name": format!("green{i}"), "status": "completed", "conclusion": "success"
            })
        });
        let routes = Routes::new(vec![
            (
                "/check-runs?per_page=100&page=2",
                serde_json::json!({
                    "check_runs": [
                        { "name": "test", "status": "completed", "conclusion": "failure" },
                    ]
                }),
            ),
            (
                "/check-runs",
                serde_json::json!({ "check_runs": serde_json::Value::Array(green) }),
            ),
            ("/status", serde_json::json!({ "statuses": [] })),
        ]);
        let session = GithubSession::for_tests(routes, token);
        let reports = checks_for(&session, "o/r", "aaaa111").await.unwrap();
        assert_eq!(
            reduce(&reports),
            CiRollup::Failed {
                failing: vec!["test".into()]
            },
        );
    }

    #[tokio::test]
    async fn a_legacy_status_beyond_the_first_page_still_counts() {
        let green = full_page(
            |i| serde_json::json!({ "context": format!("ci/green{i}"), "state": "success" }),
        );
        let routes = Routes::new(vec![
            ("/check-runs", serde_json::json!({ "check_runs": [] })),
            (
                "/status?per_page=100&page=2",
                serde_json::json!({
                    "statuses": [ { "context": "ci/legacy", "state": "error" } ]
                }),
            ),
            (
                "/status",
                serde_json::json!({ "statuses": serde_json::Value::Array(green) }),
            ),
        ]);
        let session = GithubSession::for_tests(routes, token);
        assert_eq!(
            reduce(&checks_for(&session, "o/r", "aaaa111").await.unwrap()),
            CiRollup::Failed {
                failing: vec!["ci/legacy".into()]
            },
        );
    }

    #[tokio::test]
    async fn a_later_page_that_fails_fails_the_whole_read() {
        // A prefix of a collection must never be reported as the
        // collection: "nothing failed" is what it would read as.
        struct FailsOnPage2(std::sync::Arc<std::sync::Mutex<Vec<String>>>);
        impl HttpSend for FailsOnPage2 {
            async fn send(
                &self,
                url: &str,
                _bearer: &str,
                _etag: Option<&str>,
            ) -> anyhow::Result<MiniResponse> {
                self.0.lock().unwrap().push(url.to_string());
                if url.contains("page=2") {
                    return Ok(MiniResponse {
                        status: 502,
                        headers: Default::default(),
                        body: "upstream".into(),
                    });
                }
                let page1: Vec<serde_json::Value> = (0..PER_PAGE)
                    .map(|i| {
                        serde_json::json!({
                            "name": format!("green{i}"),
                            "status": "completed",
                            "conclusion": "success"
                        })
                    })
                    .collect();
                Ok(MiniResponse {
                    status: 200,
                    headers: Default::default(),
                    body: serde_json::json!({ "check_runs": page1 }).to_string(),
                })
            }
        }
        let session = GithubSession::for_tests(FailsOnPage2(Default::default()), token);
        let err = checks_for(&session, "o/r", "aaaa111")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("502"), "{err}");
        assert!(err.contains("page=2"), "the page that failed: {err}");
    }

    #[tokio::test]
    async fn a_collection_that_never_ends_is_refused_not_truncated() {
        // Every page full, forever. Stopping at the cap and returning
        // what was read would report a build green on a prefix of its
        // checks — the cap has to be an error, not a limit.
        struct EndlessPages(std::sync::Arc<std::sync::atomic::AtomicUsize>);
        impl HttpSend for EndlessPages {
            async fn send(
                &self,
                _url: &str,
                _bearer: &str,
                _etag: Option<&str>,
            ) -> anyhow::Result<MiniResponse> {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let page: Vec<serde_json::Value> = (0..PER_PAGE)
                    .map(|i| serde_json::json!({ "number": i + 1, "head": { "sha": "x" } }))
                    .collect();
                Ok(MiniResponse {
                    status: 200,
                    headers: Default::default(),
                    body: serde_json::Value::Array(page).to_string(),
                })
            }
        }
        let calls: std::sync::Arc<std::sync::atomic::AtomicUsize> = Default::default();
        let session = GithubSession::for_tests(EndlessPages(calls.clone()), token);
        let err = open_prs(&session, "o/r").await.unwrap_err().to_string();
        assert!(err.contains("partial"), "{err}");
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            MAX_COLLECTION_PAGES as usize,
            "it reads to the cap, then refuses",
        );
    }

    #[tokio::test]
    async fn a_response_without_the_collection_is_an_error_not_an_empty_one() {
        // Silence and "nothing failed" must not be the same answer.
        let routes = Routes::new(vec![(
            "/check-runs",
            serde_json::json!({ "message": "gone" }),
        )]);
        let session = GithubSession::for_tests(routes, token);
        let err = checks_for(&session, "o/r", "aaaa111")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("check_runs"), "{err}");
    }

    fn wake(pr: u64, head: &str, kind: GithubEventKind) -> CiWake {
        let rollup = match kind {
            GithubEventKind::CiFailed => CiRollup::Failed {
                failing: vec!["fmt".into()],
            },
            _ => CiRollup::Passed,
        };
        let head = PrHead {
            number: pr,
            head: head.into(),
        };
        CiWake {
            pr,
            head: head.head.clone(),
            kind,
            action: classified("o/r", &head, &rollup).unwrap(),
        }
    }

    #[test]
    fn a_rerun_that_goes_red_green_red_says_red_once() {
        // Remembering only the LATEST conclusion makes the second red
        // look new — codex's probe turned 2002 wakes into 3003. Both
        // conclusions for a head are remembered.
        let scope = std::collections::BTreeSet::from([7]);
        let mut h = CiHorizon::default();
        assert!(
            h.absorb(vec![wake(7, "head1", GithubEventKind::CiFailed)], &scope)
                .is_empty(),
            "the first read is the state of the world",
        );
        assert_eq!(
            h.absorb(vec![wake(7, "head1", GithubEventKind::CiPassed)], &scope)
                .len(),
            1,
            "going green on that head is news",
        );
        assert!(
            h.absorb(vec![wake(7, "head1", GithubEventKind::CiFailed)], &scope)
                .is_empty(),
            "red on this head has already been said",
        );
    }

    #[test]
    fn a_new_head_is_a_new_build() {
        let scope = std::collections::BTreeSet::from([7]);
        let mut h = CiHorizon::default();
        h.absorb(vec![wake(7, "head1", GithubEventKind::CiFailed)], &scope);
        assert_eq!(
            h.absorb(vec![wake(7, "head2", GithubEventKind::CiFailed)], &scope)
                .len(),
            1,
            "nothing said about the old head applies to the new one",
        );
    }

    #[test]
    fn a_pr_out_of_scope_is_forgotten() {
        let mut h = CiHorizon::default();
        h.absorb(
            vec![
                wake(7, "head7", GithubEventKind::CiFailed),
                wake(9, "head9", GithubEventKind::CiFailed),
            ],
            &std::collections::BTreeSet::from([7, 9]),
        );
        // 9 leaves the scope, then comes back the same way it was.
        h.absorb(vec![], &std::collections::BTreeSet::from([7]));
        assert_eq!(
            h.absorb(
                vec![wake(9, "head9", GithubEventKind::CiFailed)],
                &std::collections::BTreeSet::from([7, 9])
            )
            .len(),
            1,
            "a PR that left and returned is news again, and its memory is not kept forever",
        );
    }

    #[test]
    fn a_horizon_survives_a_round_trip_through_its_file() {
        let dir = std::env::temp_dir().join(format!("clank-ci-horizon-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("h.json");
        let scope = std::collections::BTreeSet::from([7]);
        let mut h = CiHorizon::default();
        h.absorb(vec![wake(7, "head1", GithubEventKind::CiFailed)], &scope);
        h.save(Some(&path));

        let mut again = CiHorizon::load(Some(&path));
        assert!(again.is_established());
        assert!(
            again
                .absorb(vec![wake(7, "head1", GithubEventKind::CiFailed)], &scope)
                .is_empty(),
            "what it had already said survives the restart",
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_or_unreadable_file_starts_clean_and_silent() {
        let missing = std::path::Path::new("/nonexistent/clank/horizon.json");
        let mut h = CiHorizon::load(Some(missing));
        assert!(!h.is_established());
        assert!(
            h.absorb(
                vec![wake(7, "head1", GithubEventKind::CiFailed)],
                &std::collections::BTreeSet::from([7])
            )
            .is_empty(),
            "losing the memory re-establishes quietly, it does not announce old builds",
        );
        h.save(Some(missing)); // must not panic
    }

    #[test]
    fn the_key_is_the_pr_the_head_and_the_way_it_went() {
        let red = action_key(GithubEventKind::CiFailed, 7, "aaaa111");
        assert_eq!(red, "ci_failed#7@aaaa111");
        assert_ne!(red, action_key(GithubEventKind::CiPassed, 7, "aaaa111"));
        assert_ne!(red, action_key(GithubEventKind::CiFailed, 7, "bbbb222"));
        assert_ne!(red, action_key(GithubEventKind::CiFailed, 9, "aaaa111"));
    }

    fn report(name: &str, state: CheckState) -> CheckReport {
        CheckReport {
            name: name.to_string(),
            state,
        }
    }

    #[test]
    fn nothing_reporting_is_not_a_pending_build() {
        assert_eq!(reduce(&[]), CiRollup::NoChecks);
    }

    /// `stale` is GitHub marking a check that took too long, NOT a
    /// run about some older head — the reader asks for one commit's
    /// checks, so everything it returns belongs to that commit. Read
    /// as "superseded" and dropped, a stale-only PR loses its wake
    /// and a stale-plus-green PR is announced as passing on a check
    /// that never succeeded (codex 008eb13).
    #[test]
    fn a_stale_check_is_a_check_that_did_not_pass() {
        let body = serde_json::json!([
            { "name": "test", "status": "completed", "conclusion": "stale" },
        ]);
        assert_eq!(
            reduce(&reports_from_check_runs(body.as_array().unwrap())),
            CiRollup::Failed {
                failing: vec!["test".into()]
            },
            "stale alone is a failed build, not an absence of one",
        );

        let with_green = serde_json::json!([
            { "name": "test", "status": "completed", "conclusion": "stale" },
            { "name": "lint", "status": "completed", "conclusion": "success" },
        ]);
        assert_eq!(
            reduce(&reports_from_check_runs(with_green.as_array().unwrap())),
            CiRollup::Failed {
                failing: vec!["test".into()]
            },
            "one green check does not make the PR green",
        );
    }

    #[test]
    fn one_outstanding_check_holds_the_whole_result() {
        assert_eq!(
            reduce(&[
                report("fmt", CheckState::Passed),
                report("test", CheckState::Pending),
            ]),
            CiRollup::Pending,
            "a PR is not green while a check is still running",
        );
    }

    #[test]
    fn failure_does_not_wait_for_the_stragglers() {
        assert_eq!(
            reduce(&[
                report("fmt", CheckState::Failed),
                report("test", CheckState::Pending),
            ]),
            CiRollup::Failed {
                failing: vec!["fmt".into()]
            },
        );
    }

    #[test]
    fn every_failing_check_is_named_for_the_wake() {
        assert_eq!(
            reduce(&[
                report("fmt", CheckState::Failed),
                report("test", CheckState::Passed),
                report("clippy", CheckState::Failed),
            ]),
            CiRollup::Failed {
                failing: vec!["fmt".into(), "clippy".into()]
            },
        );
    }

    #[test]
    fn green_is_every_check_reporting_and_none_failing() {
        assert_eq!(
            reduce(&[
                report("fmt", CheckState::Passed),
                report("skipped-on-docs", CheckState::Passed),
            ]),
            CiRollup::Passed,
        );
    }

    #[test]
    fn a_terminal_state_that_is_not_success_is_a_failure() {
        // A PR whose only run was cancelled or timed out is not one to
        // call green — GitHub's own merge box shows it red.
        for conclusion in [
            "failure",
            "timed_out",
            "action_required",
            "cancelled",
            "stale",
        ] {
            assert_eq!(
                check_run_state("completed", Some(conclusion)),
                CheckState::Failed,
                "{conclusion}",
            );
        }
        for conclusion in ["success", "neutral", "skipped"] {
            assert_eq!(
                check_run_state("completed", Some(conclusion)),
                CheckState::Passed,
                "{conclusion}",
            );
        }
    }

    #[test]
    fn an_unfinished_or_unrecognized_run_is_pending() {
        for status in ["queued", "in_progress", "waiting", "requested"] {
            assert_eq!(
                check_run_state(status, None),
                CheckState::Pending,
                "{status}"
            );
        }
        assert_eq!(
            check_run_state("queued", Some("success")),
            CheckState::Pending,
            "a conclusion means nothing until the run is completed",
        );
        assert_eq!(
            check_run_state("completed", None),
            CheckState::Pending,
            "completed with no conclusion is not a result to wake on",
        );
        assert_eq!(
            check_run_state("completed", Some("something_new")),
            CheckState::Pending,
            "a state this binary does not know is never a wake",
        );
    }

    #[test]
    fn commit_statuses_map_the_older_vocabulary() {
        assert_eq!(commit_status_state("success"), CheckState::Passed);
        assert_eq!(commit_status_state("failure"), CheckState::Failed);
        assert_eq!(commit_status_state("error"), CheckState::Failed);
        assert_eq!(commit_status_state("pending"), CheckState::Pending);
        assert_eq!(commit_status_state("unheard-of"), CheckState::Pending);
    }

    #[test]
    fn both_surfaces_normalize_into_one_list() {
        let runs = serde_json::json!([
            { "name": "test", "status": "completed", "conclusion": "failure" },
            { "name": "fmt", "status": "in_progress", "conclusion": null },
            { "name": "no-status-field" },
        ]);
        let statuses = serde_json::json!([
            { "context": "ci/legacy", "state": "success" },
            { "state": "success" },
            { "context": "ci/mystery" },
        ]);
        let mut reports = reports_from_check_runs(runs.as_array().unwrap());
        reports.extend(reports_from_commit_statuses(statuses.as_array().unwrap()));
        assert_eq!(
            reports,
            vec![
                report("test", CheckState::Failed),
                report("fmt", CheckState::Pending),
                // Named, but with no status to read: pending, not
                // absent — it is a check, and it has not passed.
                report("no-status-field", CheckState::Pending),
                report("ci/legacy", CheckState::Passed),
                // The converse: a state this DOES understand, on a
                // context it cannot name. The rule is about the
                // state, so this one counts as the pass it says.
                report("(unreadable status)", CheckState::Passed),
                // …and a context with no state at all: pending.
                report("ci/mystery", CheckState::Pending),
            ],
            "nothing is dropped; an unreadable STATE is what holds the result pending",
        );
        assert_eq!(
            reduce(&reports),
            CiRollup::Failed {
                failing: vec!["test".into()]
            },
            "and an unreadable entry is never named as a failure",
        );
    }

    // ── session auth: sharing + uniform 401 recovery ──────────

    /// A CI reader that finds nothing — the horizon stays cold and
    /// the feed's own tests are untouched by it.
    pub(in crate::cli::github) struct NoCi;

    impl CiReader for NoCi {
        async fn read(
            &self,
        ) -> anyhow::Result<
            Vec<(
                crate::cli::github::ci::PrHead,
                crate::cli::github::ci::CiRollup,
            )>,
        > {
            Ok(Vec::new())
        }
    }

    /// A CI reader scripted tick by tick; exhausted → the last
    /// result repeats, which is what an unchanged build looks like.
    pub(in crate::cli::github) struct ScriptCi(
        Mutex<std::collections::VecDeque<Vec<(ci::PrHead, ci::CiRollup)>>>,
    );

    impl ScriptCi {
        pub(in crate::cli::github) fn new(rounds: Vec<Vec<(ci::PrHead, ci::CiRollup)>>) -> Self {
            Self(Mutex::new(rounds.into()))
        }
    }

    impl CiReader for ScriptCi {
        async fn read(&self) -> anyhow::Result<Vec<(ci::PrHead, ci::CiRollup)>> {
            let mut q = self.0.lock().unwrap();
            let next = if q.len() > 1 {
                q.pop_front().unwrap()
            } else {
                q.front().cloned().unwrap_or_default()
            };
            Ok(next)
        }
    }

    /// A reader that always fails — the CI horizon is broken, the
    /// feed's is not.
    pub(in crate::cli::github) struct DeadCi;

    impl CiReader for DeadCi {
        async fn read(&self) -> anyhow::Result<Vec<(ci::PrHead, ci::CiRollup)>> {
            anyhow::bail!("checks unreachable")
        }
    }

    pub(in crate::cli::github) fn red(
        number: u64,
        head: &str,
        job: &str,
    ) -> (ci::PrHead, ci::CiRollup) {
        (
            ci::PrHead {
                number,
                head: head.into(),
            },
            ci::CiRollup::Failed {
                failing: vec![job.into()],
            },
        )
    }

    /// A selection written the flat way, for the tests that are about
    /// which KINDS a watch wants.
    fn flat(kinds: &[GithubEventKind]) -> Selection {
        Selection::of(&crate::cli::github::feed::tests::src(kinds, false))
    }

    fn nested(watch: &str) -> Selection {
        let mut s = crate::cli::github::feed::tests::src(&[], false);
        s.watch = serde_json::from_str(watch).expect("the clauses parse");
        assert_eq!(s.refusal(), None, "{watch}");
        Selection::of(&s)
    }

    #[tokio::test]
    async fn a_watch_that_wants_only_red_is_never_woken_by_green() {
        let results = vec![
            (
                ci::PrHead {
                    number: 7,
                    head: "head1".into(),
                },
                ci::CiRollup::Passed,
            ),
            red(9, "head2", "fmt"),
        ];
        let only_red = ci::wakes("o/r", &flat(&[GithubEventKind::CiFailed]), &results);
        assert_eq!(only_red.len(), 1);
        assert_eq!(only_red[0].action.key.as_deref(), Some("ci_failed#9@head2"));
        let both = ci::wakes(
            "o/r",
            &flat(&[GithubEventKind::CiFailed, GithubEventKind::CiPassed]),
            &results,
        );
        assert_eq!(both.len(), 2);
        assert!(
            ci::wakes("o/r", &flat(&[GithubEventKind::PrOpened]), &results).is_empty(),
            "a watch with no ci kinds reads no ci wakes",
        );
    }

    /// A conclusion is delivered only where a clause asked for THAT
    /// event on THAT PR.
    ///
    /// Kind membership alone said yes to every crossing: with red
    /// wanted on one PR and green on another, a green on the first and
    /// a red on the second were both delivered, though no clause had
    /// asked for either (codex bcda3c8).
    #[tokio::test]
    async fn a_conclusion_crossed_with_the_wrong_pr_is_not_a_wake() {
        let green = |number: u64, head: &str| {
            (
                ci::PrHead {
                    number,
                    head: head.into(),
                },
                ci::CiRollup::Passed,
            )
        };
        let sel = nested(
            r#"[{"pr": {"numbers": [603], "events": ["ci_passed"]}},
                {"pr": {"numbers": [604], "events": ["ci_failed"]}}]"#,
        );
        let keys = |results: &[(ci::PrHead, ci::CiRollup)]| {
            ci::wakes("o/r", &sel, results)
                .into_iter()
                .filter_map(|w| w.action.key)
                .collect::<Vec<_>>()
        };

        // What each clause asked for.
        assert_eq!(
            keys(&[green(603, "a"), red(604, "b", "fmt")]),
            ["ci_passed#603@a", "ci_failed#604@b"],
        );
        // And the crossings, which no clause asked for.
        assert!(
            keys(&[red(603, "a", "fmt"), green(604, "b")]).is_empty(),
            "red on the PR that asked for green, and green on the one that asked for red",
        );

        // The reading that bounds the work still covers both PRs, so
        // nothing is dropped before the clauses get to judge it.
        assert_eq!(sel.ci_scope(), vec![603, 604]);
    }
}
