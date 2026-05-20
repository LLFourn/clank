//! `wait_for_work` long-poll: block until the named plan needs the caller's
//! role, then return a `WorkPayload` — the same shape `work_context`
//! embeds as its `work` prefix. Both surfaces project state through
//! `crate::responses::build_work_payload`, so the work-half is identical
//! by construction.
//!
//! Single-plan focus: the caller names `plan_id` (or lets the daemon
//! infer it from cwd-repo when there's exactly one active visible plan)
//! so the response is always for one plan in one repo — no fan-out, no
//! cross-repo. The response is one of the `ExpectedAction` variants
//! (tagged by `kind` on the wire), with action-specific paths /
//! target_sha on the variant payload itself — no separate `locations`
//! array.
//!
//! Lock boundary: resolve the plan and snapshot its cheap gate state under
//! the runtime mutex, release the lock, then per-poll read
//! `plan_worktree_status` from disk and derive `waiting_on`. Status-driven
//! master waits stay correct without holding the mutex across disk I/O.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde::Deserialize;
use tokio::sync::broadcast::error::RecvError;

use crate::lifecycle::{AgentLabel, CommitSha, ContentHash, PlanKey};
use crate::projection::waiting_on;
use crate::repo_state::{Trinity, WaitingReason, WaitingRole};
use crate::responses::compute_plan_worktree_status_parts;
use crate::review_state::CommitGate;
use crate::runtime::Runtime;
use trinity_core::api::{WaitTimeout, WorkPayload};

const DEFAULT_TIMEOUT_SECS: u64 = 1800;

#[derive(Debug, Deserialize)]
pub struct WaitArgs {
    pub role: WaitingRole,
    /// Optional plan filter. Phase 3 of `commit-first-review-model`
    /// makes this a real filter, not the primary matching key: when
    /// `None`, the matcher walks `RepoState.commits` chronologically
    /// and returns the next reviewable commit needing the caller's
    /// role. When `Some(p)`, the same chronological walk applies but
    /// only commits whose `CommitNode.plans` contains `p` count.
    ///
    /// Empty-string values are coerced to `None` for wire
    /// compatibility (the existing shim contract serializes the
    /// missing case as `""`).
    #[serde(default, deserialize_with = "deserialize_empty_as_none")]
    pub plan_id: Option<String>,
    /// Optional repo scope. Required when `plan_id` is `None` so the
    /// matcher knows which repo to walk. Accepted as a basename
    /// (looked up in `repo_basenames`) or an absolute path
    /// (canonicalized via `dunce`).
    #[serde(default)]
    pub repo: Option<String>,
    /// Optional on the wire so schema-strict MCP clients allow the shim
    /// to autofill from its cache; the daemon rejects calls that arrive
    /// without one once autofill has had its chance.
    #[serde(default)]
    pub author_label: Option<String>,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

/// Coerce `""` to `None` at deserialization. Lets the wire keep
/// emitting `""` for "no plan filter" (shim back-compat) while the
/// Rust model uses a clean `Option<String>` with no empty-string
/// sentinel.
fn deserialize_empty_as_none<'de, D>(d: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let opt = Option::<String>::deserialize(d)?;
    Ok(opt.filter(|s| !s.trim().is_empty()))
}

/// Re-export `WaitForWorkResponse` from the wire crate as `WaitResponse`
/// so existing call sites need no rename. The wire shape is identical;
/// this module is now a thin builder over the typed DTO.
pub use trinity_core::api::WaitForWorkResponse as WaitResponse;

#[derive(Debug, thiserror::Error)]
pub enum WaitError {
    #[error("invalid role: {0} (expected `master` or `reviewers`)")]
    InvalidRole(String),
    #[error("repo is required when plan_id is omitted")]
    MissingRepoForRepoScope,
    #[error("author_label is required")]
    MissingAuthorLabel,
    #[error("invalid plan_id: {0}")]
    InvalidPlanId(String),
    #[error("invalid author_label: {0}")]
    InvalidAuthorLabel(String),
    #[error("unknown repo basename: {0}")]
    UnknownRepo(String),
    #[error("unknown plan: {0}")]
    UnknownPlan(String),
    #[error("plan conflict: stem `{key}` maps to {paths:?}")]
    PlanConflict { key: PlanKey, paths: Vec<PathBuf> },
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Block until the named plan needs the caller's role. Returns the work
/// + locations to act on. After `timeout_secs` returns
///   `{timed_out: true}` with no work.
pub async fn wait_for_work(runtime: &Runtime, args: WaitArgs) -> Result<WaitResponse, WaitError> {
    let role = args.role;
    let plan_filter = parse_plan_filter(&args)?;
    let author_label = args
        .author_label
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or(WaitError::MissingAuthorLabel)?;
    let author = AgentLabel::parse(author_label)
        .map_err(|e| WaitError::InvalidAuthorLabel(e.to_string()))?;

    let timeout = derive_timeout(args.timeout_secs);
    let started_at = Instant::now();
    let mut rx = runtime.subscribe_events();

    if let Some(m) = compute_match(runtime, &plan_filter, role, &author).await? {
        let payload = enrich_for_wait(runtime, m, role, &author).await;
        return Ok(WaitResponse::Work(payload));
    }

    let deadline = started_at + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(WaitResponse::Timeout(WaitTimeout {
                timed_out: true,
                no_active_plans: false,
                repo: None,
            }));
        }
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Ok(_event)) => {
                if let Some(m) = compute_match(runtime, &plan_filter, role, &author).await? {
                    let payload = enrich_for_wait(runtime, m, role, &author).await;
                    return Ok(WaitResponse::Work(payload));
                }
            }
            Ok(Err(RecvError::Lagged(_))) => {
                rx = runtime.subscribe_events();
                if let Some(m) = compute_match(runtime, &plan_filter, role, &author).await? {
                    let payload = enrich_for_wait(runtime, m, role, &author).await;
                    return Ok(WaitResponse::Work(payload));
                }
            }
            Ok(Err(RecvError::Closed)) | Err(_) => {
                return Ok(WaitResponse::Timeout(WaitTimeout {
                    timed_out: true,
                    no_active_plans: false,
                    repo: None,
                }));
            }
        }
    }
}

/// Resolve the (plan_id, repo) inputs into a `PlanFilter`. Returns
/// `PlanFilter` directly — no `Option<PlanFilter>` masquerading as
/// "maybe absent" since the matcher always operates against a
/// concrete scope.
///
/// - `plan_id` present → `PlanFilter::Plan(parsed_id)`. (The plan id
///   itself carries the repo via `<basename>/<stem>.md`.)
/// - `plan_id` absent, `repo` present → `PlanFilter::RepoScope`.
/// - both absent → `WaitError::MissingRepoForRepoScope`.
fn parse_plan_filter(args: &WaitArgs) -> Result<PlanFilter, WaitError> {
    if let Some(s) = args.plan_id.as_deref() {
        let id = crate::lifecycle::PlanId::parse(s)
            .map_err(|e| WaitError::InvalidPlanId(e.to_string()))?;
        return Ok(PlanFilter::Plan(id));
    }
    match args.repo.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(repo) => Ok(PlanFilter::RepoScope(repo.to_string())),
        None => Err(WaitError::MissingRepoForRepoScope),
    }
}

/// Where the matcher should look for work. Phase 3 of
/// `commit-first-review-model` makes this a real two-variant
/// `enum` rather than an `Option<PlanFilter>`: every wait targets
/// either one specific plan or the whole repo's commit stream.
#[derive(Debug, Clone)]
pub(crate) enum PlanFilter {
    Plan(crate::lifecycle::PlanId),
    RepoScope(String),
}

/// Derive the long-poll duration from the caller's `timeout_secs`.
/// Defaults to `DEFAULT_TIMEOUT_SECS` (1800 / 30 min) on `None`;
/// floors at 1s; no upper cap (phase 11).
fn derive_timeout(timeout_secs: Option<u64>) -> Duration {
    Duration::from_secs(timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS).max(1))
}

/// What `compute_match` returns: the projected work payload plus the
/// post-projection bits `enrich_for_wait` needs (canonical repo root
/// for cache keying, plan key for stale-review timeline walking). The
/// payload itself carries plan-id as a string; we hand back the parsed
/// `PlanKey` here so callers don't re-parse.
pub(crate) struct WaitMatch {
    pub payload: WorkPayload,
    pub repo_root: std::path::PathBuf,
    pub plan_key: PlanKey,
    /// SHA of the latest reviewable commit, when one exists. This is
    /// the single source of truth for "current target": current-cycle
    /// reviews target it, stale reviews are everything but it. Used
    /// directly by `collect_stale_reviews` so no second timeline scan
    /// can disagree with `candidate.review_target`.
    pub current_target_sha: Option<CommitSha>,
}

/// Walk the repo's commit stream in fold order (the first-parent
/// sequence the fold built — never derived from `author_ts`) and
/// return the first commit whose gate needs the caller's role
/// (and that the caller has not already voted on, for reviewer
/// waits). Phase 3 of `commit-first-review-model`: the reviewable
/// unit is the commit, not the plan, so the matcher iterates
/// `RepoState.commit_order` instead of fanning out over plans by
/// name.
///
/// - `PlanFilter::Plan(id)` requests a single-plan candidate from
///   that plan's latest reviewable commit.
/// - `PlanFilter::RepoScope(repo)` walks the full commit_order
///   stream and emits one candidate per active plan at its latest
///   reviewable commit, ordered by fold position.
///
/// Per-plan supersession is enforced inside
/// `collect_repo_scope_candidates`: within a plan, only the
/// LATEST reviewable commit's gate drives work. The
/// supersession rule is documented inline there.
async fn compute_match(
    runtime: &Runtime,
    filter: &PlanFilter,
    role: WaitingRole,
    author: &AgentLabel,
) -> Result<Option<WaitMatch>, WaitError> {
    let candidates = {
        let trinity_arc = runtime.state();
        let trinity = trinity_arc.lock().await;
        collect_active_commit_candidates(&trinity, filter)?
    };

    for candidate in candidates {
        if let Some(m) = try_match_candidate(candidate, role, author)? {
            return Ok(Some(m));
        }
    }
    Ok(None)
}

/// Per-candidate matching: status check, role gate, caller-already-
/// voted guard, payload build. Returns the produced `WaitMatch` or
/// `None` if this candidate doesn't carry work for the caller.
fn try_match_candidate(
    candidate: Candidate,
    role: WaitingRole,
    author: &AgentLabel,
) -> Result<Option<WaitMatch>, WaitError> {
    let status = compute_plan_worktree_status_parts(
        &candidate.repo_root,
        &candidate.plan_path,
        &candidate.body_hash,
    )?;
    if !candidate.is_finished
        && matches!(
            status,
            crate::repo_state::PlanWorktreeStatus::PlanFileMissing
        )
    {
        return Ok(None);
    }
    let w = waiting_on(candidate.is_finished, status, candidate.gate.as_ref());
    let terminal = matches!(w.reason, WaitingReason::SessionFinished);
    if !terminal && w.role != role {
        return Ok(None);
    }
    if matches!(role, WaitingRole::Reviewers) && caller_already_voted(&candidate, w.reason, author)
    {
        return Ok(None);
    }

    let current_reviews = candidate
        .gate
        .as_ref()
        .map(crate::responses::current_reviews_from_gate)
        .unwrap_or_default();
    let payload = crate::responses::build_work_payload(crate::responses::WorkPayloadInputs {
        plan_id: &candidate.plan_id_str,
        repo_root: &candidate.repo_root,
        plan_key: &candidate.plan_key,
        plan_path: &candidate.plan_path,
        waiting: &w,
        review_target_sha: candidate.review_target.as_ref(),
        review_target_kind: candidate.review_target_kind,
        current_reviews: &current_reviews,
        author,
    });
    Ok(Some(WaitMatch {
        payload,
        repo_root: candidate.repo_root,
        plan_key: candidate.plan_key,
        current_target_sha: candidate.review_target,
    }))
}

/// Cap for inlined body content. Larger files emit `content:
/// None` and rely on the agent to `Read` separately. The hash is
/// still cached so the file's seen-state survives — subsequent polls
/// still read and hash to detect changes, but skip re-sending content
/// when the hash matches.
const MAX_INLINE_BODY: usize = 64 * 1024;

/// Wrap `WorkPayload` in `WaitWorkPayload`, opportunistically
/// filling current-cycle `content` (first-encounter, hash-keyed) and
/// collecting the master-only `stale_reviews` sidecar (one-shot,
/// path-keyed).
///
/// Lock discipline: file reads + hashing run OUTSIDE the runtime
/// mutex. Cache query/mark take the lock briefly. Concurrent pollers
/// for the same key are idempotent — both may read and mark; both
/// deliver `content`. Don't serialize the disk read; that would
/// re-lock during I/O.
///
/// `stale_reviews` rides along on legitimate master wakeups only —
/// it never wakes WFW on its own. Reviewer-bound wakeups get an
/// empty sidecar; SessionFinished delivered to a reviewer also gets
/// an empty sidecar (stale reviews are a master concern).
async fn enrich_for_wait(
    runtime: &Runtime,
    m: WaitMatch,
    role: WaitingRole,
    author: &AgentLabel,
) -> trinity_core::api::WaitWorkPayload {
    use trinity_core::api::ExpectedAction as A;
    let WaitMatch {
        mut payload,
        repo_root,
        plan_key,
        current_target_sha,
    } = m;

    match &mut payload.action {
        A::WriteFeedback { plan_file, .. } => {
            let trinity_core::api::PlanFile { path, content } = plan_file;
            opportunistic_fill(runtime, &repo_root, author, path, content).await;
        }
        A::AddressChanges { reviews, .. } => {
            for r in reviews.iter_mut() {
                let trinity_core::api::CurrentReview { path, content, .. } = r;
                opportunistic_fill(runtime, &repo_root, author, path, content).await;
            }
        }
        A::CommitPlanRevision { .. } | A::StartImplementation { .. } | A::SessionFinished => {}
    }

    let stale_reviews = if matches!(role, WaitingRole::Master) {
        collect_stale_reviews(
            runtime,
            &repo_root,
            &plan_key,
            current_target_sha.as_ref(),
            author,
        )
        .await
    } else {
        Vec::new()
    };

    trinity_core::api::WaitWorkPayload {
        work: payload,
        stale_reviews,
    }
}

/// Walk the plan timeline for feedback against superseded (non-
/// current-target) commits and emit each one exactly once to
/// `author`. Reservation is atomic with the check: each candidate's
/// cache key goes into `seen_stale_reviews` via `BTreeSet::insert`
/// while we still hold the runtime lock. If `insert` returns false
/// the entry was already claimed (by a prior poll OR a concurrent
/// poll), so we skip it. Disk reads happen outside the lock; a
/// reservation that later fails to read just yields
/// `content: None` — "delivered with metadata, body unreadable" is
/// the same outcome as "oversized" by design.
async fn collect_stale_reviews(
    runtime: &Runtime,
    repo_root: &std::path::Path,
    plan_key: &PlanKey,
    current_target_sha: Option<&CommitSha>,
    author: &AgentLabel,
) -> Vec<trinity_core::api::StaleReview> {
    let candidates: Vec<(String, AgentLabel, trinity_core::Verdict, String)> = {
        let trinity_arc = runtime.state();
        let mut trinity = trinity_arc.lock().await;
        let timeline_snapshot: Vec<_> = match trinity.repos.get(repo_root) {
            Some(state) => match state.plans.get(plan_key) {
                Some(plan) => plan
                    .timeline
                    .iter()
                    .filter_map(|event| {
                        let sha = event.sha();
                        let gate = state.gate_for(sha)?;
                        let feedback: Vec<_> = gate
                            .feedback
                            .iter()
                            .map(|(a, fb)| (a.clone(), fb.verdict))
                            .collect();
                        Some((sha.as_str().to_string(), feedback))
                    })
                    .collect(),
                None => return Vec::new(),
            },
            None => return Vec::new(),
        };
        let current_target = current_target_sha.map(CommitSha::as_str);
        let mut out = Vec::new();
        for (sha, feedback) in timeline_snapshot {
            if Some(sha.as_str()) == current_target {
                continue;
            }
            for (review_author, verdict) in feedback {
                if &review_author == author {
                    continue;
                }
                let path = crate::disk_format::feedback_path_wire(plan_key, &sha, &review_author);
                let key = (repo_root.to_path_buf(), author.clone(), path.clone());
                if !trinity.seen_stale_reviews.insert(key) {
                    continue;
                }
                out.push((path, review_author, verdict, sha.clone()));
            }
        }
        out
    };

    let mut result = Vec::with_capacity(candidates.len());
    for (path, review_author, verdict, target_sha) in candidates {
        let abs = repo_root.join(&path);
        let content = match tokio::fs::read_to_string(&abs).await {
            Ok(body) if body.len() <= MAX_INLINE_BODY => Some(body),
            _ => None,
        };
        result.push(trinity_core::api::StaleReview {
            path,
            author: review_author,
            verdict,
            target_sha,
            content,
        });
    }
    result
}

/// Read the file at `repo_root/path`, hash it, check the
/// opportunistic-body cache. On cache hit (same hash already
/// sent) leave `content` `None`. On miss or hash mismatch, fill
/// `content` (subject to the inline cap) and mark sent.
async fn opportunistic_fill(
    runtime: &Runtime,
    repo_root: &std::path::Path,
    author: &AgentLabel,
    path: &str,
    content: &mut Option<String>,
) {
    let abs = repo_root.join(path);
    let body = match tokio::fs::read_to_string(&abs).await {
        Ok(b) => b,
        Err(_) => return,
    };
    let hash = crate::lifecycle::content_hash(&body);
    if runtime
        .opportunistic_body_seen(repo_root, author, path, &hash)
        .await
    {
        return;
    }
    if body.len() <= MAX_INLINE_BODY {
        *content = Some(body);
    }
    runtime
        .mark_opportunistic_body_sent(
            repo_root.to_path_buf(),
            author.clone(),
            path.to_string(),
            hash,
        )
        .await;
}

/// True if `author` already has a current-target verdict for the
/// reviewer work named by `reason`. Used to keep `wait_for_work` from
/// re-waking a reviewer for a target they've already voted on (their
/// vote still stands; the remaining wait is on someone else).
///
/// Master-role reasons return false — caller-already-voted is a
/// reviewer-only concept. The match is exhaustive: adding any new
/// `WaitingReason` forces a compile error here. **The compiler enforces
/// totality, not arm placement** — a maintainer who adds a new
/// reviewer-role variant must place it in the gate-lookup branch, not
/// the `return false` branch, or the self-wakeup bug returns. The
/// integration test `caller_already_voted_does_not_re_wake_reviewer`
/// (and its wire counterpart in `server::http::wire_tests`) is the
/// semantic guard for arm placement.
fn caller_already_voted(cand: &Candidate, reason: WaitingReason, author: &AgentLabel) -> bool {
    use WaitingReason::*;
    let gate = match reason {
        // CommitNeedsReview is the only reviewer-role reason. The gate
        // is the latest reviewable commit's gate — same one driving
        // waiting_on and review_target. Master-role reasons return
        // false; caller-already-voted is a reviewer-only concept.
        CommitNeedsReview => cand.gate.as_ref(),
        SessionFinished
        | CommitPlanRevision
        | AddressCommitChanges
        | ReadyToStartImplementation => {
            return false;
        }
    };
    let Some(gate) = gate else { return false };
    gate.approvers.contains(author)
        || gate.requesters.contains(author)
        || gate.ambiguous.contains(author)
}

#[derive(Debug, Clone)]
struct Candidate {
    repo_root: PathBuf,
    plan_key: PlanKey,
    /// Wire-form plan id `<basename>/<stem>.md`. Pre-computed so the
    /// per-candidate match path doesn't re-derive it.
    plan_id_str: String,
    plan_path: String,
    body_hash: ContentHash,
    is_finished: bool,
    /// One gate: the latest reviewable commit's gate. Folds the old
    /// (plan_gate, impl_gate) pair into the single value that drives
    /// waiting_on, locations, target_sha, and commit_kind.
    gate: Option<CommitGate>,
    /// The SHA the gate was computed on — the same SHA reviewers
    /// should write to. Equal to `latest_reviewable_commit_for`
    /// output, which skips MultiPlan / DoneMove / Unattributed.
    /// `None` when no reviewable commit exists yet.
    review_target: Option<CommitSha>,
    /// `CommitKind` of `review_target`, threaded through so callers
    /// don't re-derive it. `None` iff `review_target` is `None`.
    review_target_kind: Option<crate::repo_state::CommitKind>,
}

/// Build the chronologically-ordered candidate stream for `filter`.
///
/// For the per-plan filter, this is exactly the legacy behavior:
/// one candidate built from that plan's latest reviewable commit.
///
/// For the repo-scope filter, the algorithm walks the repo's commit
/// map and returns one candidate per active plan, ordered by the
/// timestamp of THAT plan's latest reviewable commit (oldest-first).
/// Per-plan supersession is preserved (a plan with multiple
/// reviewable commits is represented once, by its latest), and
/// across plans the order is commit-chronological rather than
/// plan-key-alphabetical. Frozen plans and plan conflicts are
/// skipped: a long-frozen plan's SessionFinished result must not
/// starve real active work.
fn collect_active_commit_candidates(
    trinity: &Trinity,
    filter: &PlanFilter,
) -> Result<Vec<Candidate>, WaitError> {
    match filter {
        PlanFilter::Plan(plan_id) => Ok(vec![collect_candidate_by_plan(trinity, plan_id)?]),
        PlanFilter::RepoScope(repo) => collect_repo_scope_candidates(trinity, repo),
    }
}

/// Walk `RepoState.commit_order` (the fold's true first-parent
/// sequence — never reconstructed from `author_ts`) and emit one
/// candidate per active plan, ordered by where that plan's LATEST
/// reviewable commit sits in the fold sequence.
///
/// **Supersession rule (documented):** repo-scope considers ONLY
/// each active plan's latest reviewable commit. A plan with three
/// reviewable commits — plan_intro at fold-index 1, revision at
/// fold-index 4, impl at fold-index 7 — surfaces ONCE as
/// "needs work on fold-index 7." The earlier reviewable commits
/// on that plan are historical: reviews exist on them, but the
/// gate that drives "what should I do next?" is the latest.
/// Phase 4 ad hoc reviewable commits join this stream as their own
/// nodes; they have no supersession (every ad hoc commit is its
/// own review unit). The explicit-plan path keeps the same rule
/// for symmetry.
fn collect_repo_scope_candidates(
    trinity: &Trinity,
    repo_ref: &str,
) -> Result<Vec<Candidate>, WaitError> {
    let (basename, repo_root) = resolve_repo_ref(trinity, repo_ref)?;
    let repo_state = trinity
        .repos
        .get(&repo_root)
        .ok_or_else(|| WaitError::UnknownRepo(repo_ref.to_string()))?;

    // Single pass over commit_order: for each reviewable
    // Plan(p)-attributed commit, overwrite the per-plan latest
    // entry. Because we walk in fold order, the last write wins
    // and naturally captures the latest reviewable commit per plan.
    let mut latest_per_plan: BTreeMap<PlanKey, CommitSha> = BTreeMap::new();
    for sha in &repo_state.commit_order {
        let Some(node) = repo_state.commits.get(sha) else {
            continue;
        };
        if node.gate.is_none() {
            continue;
        }
        let plan_key = match &node.attribution {
            crate::repo_state::CommitAttribution::Plan { plan } => plan.clone(),
            _ => continue,
        };
        if repo_state.plan_conflicts.contains_key(&plan_key) {
            continue;
        }
        let Some(plan) = repo_state.plans.get(&plan_key) else {
            continue;
        };
        if plan.is_frozen() {
            continue;
        }
        latest_per_plan.insert(plan_key, sha.clone());
    }

    // Walk commit_order AGAIN forward, emitting candidates only for
    // the SHAs that ARE the latest-reviewable for their plan. This
    // preserves the fold's chronology AND deduplicates plans (each
    // plan appears at most once in the candidate stream).
    let latest_set: std::collections::BTreeSet<&CommitSha> = latest_per_plan.values().collect();
    let mut candidates = Vec::new();
    for sha in &repo_state.commit_order {
        if !latest_set.contains(sha) {
            continue;
        }
        let Some(node) = repo_state.commits.get(sha) else {
            continue;
        };
        let plan_key = match &node.attribution {
            crate::repo_state::CommitAttribution::Plan { plan } => plan.clone(),
            _ => continue,
        };
        if let Some(plan) = repo_state.plans.get(&plan_key) {
            let plan_id = crate::lifecycle::PlanId::new(basename.clone(), plan_key);
            candidates.push(build_candidate(
                repo_root.clone(),
                plan_id,
                plan,
                repo_state,
            ));
        }
    }
    Ok(candidates)
}

/// Resolve a `repo` argument (basename `frostsnap` OR absolute path
/// `/Users/llfourn/src/frostsnap`) into a `(RepoBasename, repo_root)`
/// pair. Both spellings must map to the same underlying repo state;
/// HTTP callers commonly use absolute paths, MCP callers basename.
///
/// The function intentionally tries basename FIRST (the more common
/// form), then absolute-path canonicalization. An empty `repo_ref`
/// or one that maps to no watched repo returns `UnknownRepo`.
fn resolve_repo_ref(
    trinity: &Trinity,
    repo_ref: &str,
) -> Result<(crate::lifecycle::RepoBasename, PathBuf), WaitError> {
    if let Ok(basename) = crate::lifecycle::RepoBasename::parse(repo_ref) {
        if let Some(root) = trinity.repo_basenames.get(&basename) {
            return Ok((basename, root.clone()));
        }
    }
    // Treat as absolute path.
    let canonical = dunce::canonicalize(repo_ref).map_err(|_| {
        // Path doesn't exist or isn't canonicalizable — treat as unknown.
        WaitError::UnknownRepo(repo_ref.to_string())
    })?;
    if let Some(basename) = crate::lifecycle::RepoBasename::from_repo_root(&canonical) {
        if trinity.repo_basenames.get(&basename) == Some(&canonical) {
            return Ok((basename, canonical));
        }
    }
    Err(WaitError::UnknownRepo(repo_ref.to_string()))
}

fn collect_candidate_by_plan(
    trinity: &Trinity,
    plan_id: &crate::lifecycle::PlanId,
) -> Result<Candidate, WaitError> {
    let repo_root = trinity
        .repo_basenames
        .get(plan_id.repo())
        .ok_or_else(|| WaitError::UnknownRepo(plan_id.repo().as_str().to_string()))?
        .clone();
    let repo_state = trinity
        .repos
        .get(&repo_root)
        .ok_or_else(|| WaitError::UnknownRepo(plan_id.repo().as_str().to_string()))?;
    if let Some(paths) = repo_state.plan_conflicts.get(plan_id.key()) {
        return Err(WaitError::PlanConflict {
            key: plan_id.key().clone(),
            paths: paths.clone(),
        });
    }
    let plan = repo_state
        .plans
        .get(plan_id.key())
        .ok_or_else(|| WaitError::UnknownPlan(plan_id.to_string()))?;
    Ok(build_candidate(repo_root, plan_id.clone(), plan, repo_state))
}

fn build_candidate(
    repo_root: PathBuf,
    plan_id: crate::lifecycle::PlanId,
    plan: &crate::repo_state::Plan,
    repo_state: &crate::repo_state::RepoState,
) -> Candidate {
    let review_target = crate::projection::latest_reviewable_commit_for(plan);
    let review_target_kind = review_target
        .as_ref()
        .map(|sha| crate::projection::commit_kind_for(plan, sha));
    let gate = crate::projection::latest_reviewable_commit_gate_for(plan, repo_state).cloned();
    Candidate {
        repo_root,
        plan_key: plan.id.clone(),
        plan_id_str: plan_id.to_string(),
        plan_path: plan.plan_path.clone(),
        body_hash: plan.body_hash.clone(),
        is_finished: plan.is_frozen(),
        gate,
        review_target,
        review_target_kind,
    }
}

#[cfg(test)]
mod tests {
    //! Pure tests over `derive_locations` + `parse_role`. Lock-bound
    //! orchestration (snapshot under mutex → disk read → match) gets
    //! covered by `integration_tests`.

    use super::*;
    use crate::lifecycle::content_hash;
    use crate::review_state::CommitGateState;

    fn agents(labels: &[&str]) -> Vec<AgentLabel> {
        labels
            .iter()
            .map(|s| AgentLabel::parse(s).unwrap())
            .collect()
    }

    fn gate(
        state: CommitGateState,
        participants: Vec<AgentLabel>,
        approvers: Vec<AgentLabel>,
        requesters: Vec<AgentLabel>,
        missing: Vec<AgentLabel>,
    ) -> CommitGate {
        CommitGate {
            state,
            participants,
            approvers,
            requesters,
            ambiguous: Vec::new(),
            missing,
            feedback: std::collections::BTreeMap::new(),
        }
    }

    fn cand(target: Option<&str>, kind: Option<crate::repo_state::CommitKind>) -> Candidate {
        Candidate {
            repo_root: PathBuf::from("/repo"),
            plan_key: PlanKey::parse("sid").unwrap(),
            plan_id_str: "trinity/sid.md".to_string(),
            plan_path: ".trinity/plans/sid.md".to_string(),
            body_hash: content_hash("x"),
            is_finished: false,
            gate: None,
            review_target: target.map(|s| CommitSha::parse(s).unwrap()),
            review_target_kind: kind,
        }
    }

    fn me() -> AgentLabel {
        AgentLabel::parse("codex").unwrap()
    }

    /// Test helper: build a `WorkPayload` from a `Candidate` + reason
    /// via the shared production builder, then extract the
    /// action-specific paths as a flat `Vec<String>` matching the
    /// shape the old `derive_locations` returned. Lets the
    /// path-derivation tests keep their assertion form.
    fn derive_locations(
        cand: &Candidate,
        reason: WaitingReason,
        author: &AgentLabel,
    ) -> Vec<String> {
        use trinity_core::api::ExpectedAction::*;
        let w = trinity_core::api::WaitingOn {
            role: WaitingRole::Master,
            reason,
            agents: Vec::new(),
            description: String::new(),
        };
        let current_reviews = cand
            .gate
            .as_ref()
            .map(crate::responses::current_reviews_from_gate)
            .unwrap_or_default();
        let payload = crate::responses::build_work_payload(crate::responses::WorkPayloadInputs {
            plan_id: "trinity/sid.md",
            repo_root: &cand.repo_root,
            plan_key: &cand.plan_key,
            plan_path: &cand.plan_path,
            waiting: &w,
            review_target_sha: cand.review_target.as_ref(),
            review_target_kind: cand.review_target_kind,
            current_reviews: &current_reviews,
            author,
        });
        match payload.action {
            WriteFeedback { path, .. } => vec![path],
            AddressChanges {
                reviews, plan_path, ..
            } => {
                let mut out: Vec<String> = reviews.into_iter().map(|r| r.path).collect();
                if let Some(p) = plan_path {
                    out.push(p);
                }
                out
            }
            CommitPlanRevision { plan_path } => vec![plan_path],
            StartImplementation { plan_path, .. } => vec![plan_path],
            SessionFinished => Vec::new(),
        }
    }

    #[test]
    fn review_plan_location_is_canonical_write_path_for_caller() {
        let c = cand(
            Some("abc123"),
            Some(crate::repo_state::CommitKind::PlanOnly),
        );
        let v = derive_locations(&c, WaitingReason::CommitNeedsReview, &me());
        assert_eq!(v, vec![".trinity/feedback/sid/abc123/codex.md"]);
    }

    #[test]
    fn review_impl_location_uses_target() {
        let c = cand(
            Some("def456"),
            Some(crate::repo_state::CommitKind::CodeOnly),
        );
        let v = derive_locations(&c, WaitingReason::CommitNeedsReview, &me());
        assert_eq!(v, vec![".trinity/feedback/sid/def456/codex.md"]);
    }

    #[test]
    fn address_plan_request_changes_lists_rc_files_then_plan() {
        let g = gate(
            CommitGateState::ChangesRequested,
            agents(&["alice", "bob"]),
            Vec::new(),
            agents(&["alice", "bob"]),
            Vec::new(),
        );
        let mut c = cand(Some("a1a1"), Some(crate::repo_state::CommitKind::PlanOnly));
        c.gate = Some(g);
        let v = derive_locations(&c, WaitingReason::AddressCommitChanges, &me());
        assert_eq!(
            v,
            vec![
                ".trinity/feedback/sid/a1a1/alice.md",
                ".trinity/feedback/sid/a1a1/bob.md",
                ".trinity/plans/sid.md",
            ]
        );
    }

    #[test]
    fn address_impl_request_changes_lists_rc_files_only() {
        let g = gate(
            CommitGateState::ChangesRequested,
            agents(&["dana"]),
            Vec::new(),
            agents(&["dana"]),
            Vec::new(),
        );
        let mut c = cand(Some("1019"), Some(crate::repo_state::CommitKind::CodeOnly));
        c.gate = Some(g);
        let v = derive_locations(&c, WaitingReason::AddressCommitChanges, &me());
        assert_eq!(v, vec![".trinity/feedback/sid/1019/dana.md"]);
    }

    #[test]
    fn address_mixed_kind_treated_as_plan_side() {
        // A `Mixed` commit (plan touch + code) is plan-side for the
        // address-RC location list — the plan file goes on the end.
        let g = gate(
            CommitGateState::ChangesRequested,
            agents(&["alice"]),
            Vec::new(),
            agents(&["alice"]),
            Vec::new(),
        );
        let mut c = cand(Some("3137"), Some(crate::repo_state::CommitKind::Mixed));
        c.gate = Some(g);
        let v = derive_locations(&c, WaitingReason::AddressCommitChanges, &me());
        assert_eq!(
            v,
            vec![
                ".trinity/feedback/sid/3137/alice.md",
                ".trinity/plans/sid.md",
            ]
        );
    }

    #[test]
    fn commit_plan_revision_location_is_plan_file() {
        let c = cand(None, None);
        let v = derive_locations(&c, WaitingReason::CommitPlanRevision, &me());
        assert_eq!(v, vec![".trinity/plans/sid.md"]);
    }

    #[test]
    fn ready_to_implement_location_is_plan_file() {
        // ReadyToStartImplementation implies an approved previous
        // commit (the production projection never produces this
        // reason without a target); the new builder requires it
        // structurally. Pass an arbitrary SHA for the test.
        let c = cand(Some("abc1"), Some(crate::repo_state::CommitKind::CodeOnly));
        let v = derive_locations(&c, WaitingReason::ReadyToStartImplementation, &me());
        assert_eq!(v, vec![".trinity/plans/sid.md"]);
    }

    #[test]
    fn session_done_yields_no_locations() {
        let c = cand(None, None);
        let v = derive_locations(&c, WaitingReason::SessionFinished, &me());
        assert!(v.is_empty());
    }

    /// `role` is now a typed `WaitingRole` on `WaitArgs`, so unknown
    /// values fail at the serde deserialize step (before the daemon
    /// ever sees a `WaitArgs`). These tests pin that contract.
    #[test]
    fn role_unknown_strings_fail_to_deserialize() {
        for bad in ["reviewer", "", "MASTER", "Reviewers"] {
            let wire = serde_json::json!({
                "role": bad,
                "plan_id": "trinity/foo.md",
                "author_label": "alice",
            });
            let result: Result<WaitArgs, _> = serde_json::from_value(wire);
            assert!(result.is_err(), "role={bad:?} must fail to decode");
        }
    }

    #[test]
    fn role_canonical_strings_deserialize_to_enum() {
        for (input, want) in [
            ("master", WaitingRole::Master),
            ("reviewers", WaitingRole::Reviewers),
        ] {
            let wire = serde_json::json!({
                "role": input,
                "plan_id": "trinity/foo.md",
                "author_label": "alice",
            });
            let args: WaitArgs = serde_json::from_value(wire).unwrap();
            assert_eq!(args.role, want);
        }
    }

    #[test]
    fn derive_timeout_default_is_30_minutes() {
        assert_eq!(derive_timeout(None), Duration::from_secs(1800));
    }

    #[test]
    fn derive_timeout_above_300_is_not_clamped() {
        // Regression for the phase-11 cap removal: reintroducing
        // `.clamp(1, 300)` would change this output.
        assert_eq!(derive_timeout(Some(3600)), Duration::from_secs(3600));
        assert_eq!(derive_timeout(Some(86_400)), Duration::from_secs(86_400));
    }

    #[test]
    fn derive_timeout_floors_at_one_second() {
        assert_eq!(derive_timeout(Some(0)), Duration::from_secs(1));
    }
}

#[cfg(test)]
mod integration_tests {
    //! End-to-end against a real `Runtime` over tempdir git repos. Covers
    //! the lock-snapshot-then-disk-read orchestration plus the
    //! broadcast-driven wake-up path.

    use super::*;
    use crate::fs_watcher::FilesystemSignal;
    use crate::runtime::Runtime;
    use std::path::Path;
    use std::process::Command;

    fn init_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        run_git(path, &["init", "--quiet", "--initial-branch=main"]);
        run_git(path, &["config", "user.email", "test@test"]);
        run_git(path, &["config", "user.name", "test"]);
        run_git(path, &["config", "commit.gpgsign", "false"]);
        dir
    }

    fn run_git(cwd: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success());
    }

    fn write_file(repo: &Path, rel: &str, body: &str) {
        let abs = repo.join(rel);
        if let Some(parent) = abs.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(abs, body).unwrap();
    }

    fn commit(repo: &Path, msg: &str) {
        run_git(repo, &["add", "-A"]);
        run_git(repo, &["commit", "--quiet", "-m", msg]);
    }

    fn args(repo: &Path, role: WaitingRole, sid: &str, author: &str) -> WaitArgs {
        let basename = repo
            .file_name()
            .and_then(|n| n.to_str())
            .expect("tempdir has basename");
        WaitArgs {
            role,
            plan_id: Some(format!("{basename}/{sid}.md")),
            author_label: Some(author.to_string()),
            timeout_secs: Some(2),
            repo: None,
        }
    }

    fn expect_work(r: WaitResponse) -> (String, Vec<String>) {
        match r {
            WaitResponse::Work(p) => {
                let kind = action_kind(&p.work.action).to_string();
                let locations = action_locations(&p.work.action);
                (kind, locations)
            }
            WaitResponse::Timeout { .. } => panic!("expected work, got timeout"),
        }
    }

    fn expect_timeout(r: WaitResponse) {
        match r {
            WaitResponse::Timeout(t) => assert!(t.timed_out),
            WaitResponse::Work(p) => {
                panic!(
                    "expected timeout, got work={} locations={:?}",
                    action_kind(&p.work.action),
                    action_locations(&p.work.action)
                )
            }
        }
    }

    fn action_kind(a: &trinity_core::api::ExpectedAction) -> &'static str {
        use trinity_core::api::ExpectedAction::*;
        match a {
            WriteFeedback { .. } => "write_feedback",
            AddressChanges { .. } => "address_changes",
            CommitPlanRevision { .. } => "commit_plan_revision",
            StartImplementation { .. } => "start_implementation",
            SessionFinished => "session_finished",
        }
    }

    /// Translate variant-specific path payloads into the flat
    /// `Vec<String>` the existing tests assert against. Lets the old
    /// `expect_work() == ("write_feedback", [path])` shape keep working
    /// across the wire-payload reshape.
    fn action_locations(a: &trinity_core::api::ExpectedAction) -> Vec<String> {
        use trinity_core::api::ExpectedAction::*;
        match a {
            WriteFeedback { path, .. } => vec![path.clone()],
            AddressChanges {
                reviews, plan_path, ..
            } => {
                let mut out: Vec<String> = reviews.iter().map(|r| r.path.clone()).collect();
                if let Some(p) = plan_path {
                    out.push(p.clone());
                }
                out
            }
            CommitPlanRevision { plan_path } => vec![plan_path.clone()],
            StartImplementation { plan_path, .. } => vec![plan_path.clone()],
            SessionFinished => Vec::new(),
        }
    }

    #[tokio::test]
    async fn immediate_review_plan_after_first_commit() {
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
        commit(dir.path(), "add foo");
        let rt = Runtime::new();
        rt.add_repo(dir.path().to_path_buf()).await.unwrap();

        let resp = wait_for_work(
            &rt,
            args(dir.path(), WaitingRole::Reviewers, "foo", "codex"),
        )
        .await
        .unwrap();
        let (work, locations) = expect_work(resp);
        assert_eq!(work, "write_feedback");
        assert_eq!(locations.len(), 1);
        assert!(
            locations[0].starts_with(".trinity/feedback/foo/"),
            "got: {}",
            locations[0]
        );
        assert!(locations[0].ends_with("/codex.md"));
    }

    /// Phase 3 of commit-first-review-model: `wait_for_work` accepts
    /// an empty plan_id + a `repo` scope and fans out across the
    /// repo's active visible plans, returning the first plan needing
    /// the caller's role.
    #[tokio::test]
    async fn repo_scope_finds_first_plan_needing_role() {
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/alpha.md", "# alpha\n");
        commit(dir.path(), "intro alpha");
        write_file(dir.path(), ".trinity/plans/beta.md", "# beta\n");
        commit(dir.path(), "intro beta");
        let rt = Runtime::new();
        rt.add_repo(dir.path().to_path_buf()).await.unwrap();

        let basename = dir
            .path()
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap()
            .to_string();
        let repo_scope_args = WaitArgs {
            role: WaitingRole::Reviewers,
            plan_id: None,
            repo: Some(basename.clone()),
            author_label: Some("codex".to_string()),
            timeout_secs: Some(2),
        };
        let resp = wait_for_work(&rt, repo_scope_args).await.unwrap();
        let payload = match resp {
            WaitResponse::Work(p) => p,
            WaitResponse::Timeout(_) => panic!("expected work on repo-scope wait, got timeout"),
        };
        assert!(
            payload.work.plans.iter().any(|p| p.contains("alpha.md") || p.contains("beta.md")),
            "expected plan filter to identify alpha or beta; got plans={:?}",
            payload.work.plans
        );
    }

    /// Phase 3 (commit-first-review-model) architectural regression:
    /// repo-scope walks the COMMIT STREAM by fold order, not
    /// alphabetical plan keys. Codex's example: introduce plan `zzz`
    /// FIRST and plan `aaa` SECOND. Both have a reviewable plan_intro
    /// needing the same reviewer. The plan-key-alphabetical order
    /// would return aaa@commit-2, but the fold-order walk returns
    /// zzz@commit-1.
    #[tokio::test]
    async fn repo_scope_returns_chronologically_earliest_plan_not_alphabetical() {
        let dir = init_repo();
        // Introduce zzz FIRST.
        write_file(dir.path(), ".trinity/plans/zzz.md", "# zzz\n");
        commit(dir.path(), "intro zzz");
        write_file(dir.path(), ".trinity/plans/aaa.md", "# aaa\n");
        commit(dir.path(), "intro aaa");
        let rt = Runtime::new();
        rt.add_repo(dir.path().to_path_buf()).await.unwrap();

        let basename = dir
            .path()
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap()
            .to_string();
        let args = WaitArgs {
            role: WaitingRole::Reviewers,
            plan_id: None,
            repo: Some(basename),
            author_label: Some("codex".to_string()),
            timeout_secs: Some(2),
        };
        let resp = wait_for_work(&rt, args).await.unwrap();
        let payload = match resp {
            WaitResponse::Work(p) => p,
            WaitResponse::Timeout(_) => panic!("expected work, got timeout"),
        };
        assert!(
            payload.work.plans.iter().any(|p| p.ends_with("zzz.md")),
            "repo-scope must return chronologically-earliest plan zzz first; \
             got plans={:?} (this regresses to plan-key-alphabetical fan-out)",
            payload.work.plans
        );
    }

    /// Phase 3 architectural regression for codex on 0da8236:
    /// repo-scope MUST use the fold's first-parent commit order, not
    /// derived `author_ts` sorting. Pin the invariant by committing
    /// two plan_intros with an IDENTICAL author timestamp but
    /// distinct fold positions. The fold-order walk surfaces the
    /// one that came first in the chain; a timestamp-only sort would
    /// be non-deterministic.
    #[tokio::test]
    async fn repo_scope_walks_fold_order_not_author_ts() {
        let dir = init_repo();
        // Force both commits to share an author timestamp.
        let fixed_ts = "Mon Jan 1 00:00:00 2024 +0000";
        write_file(dir.path(), ".trinity/plans/first.md", "# first\n");
        run_git(dir.path(), &["add", "-A"]);
        run_git(
            dir.path(),
            &[
                "commit",
                "--quiet",
                "--date",
                fixed_ts,
                "-m",
                "intro first",
            ],
        );
        write_file(dir.path(), ".trinity/plans/second.md", "# second\n");
        run_git(dir.path(), &["add", "-A"]);
        run_git(
            dir.path(),
            &[
                "commit",
                "--quiet",
                "--date",
                fixed_ts,
                "-m",
                "intro second",
            ],
        );
        let rt = Runtime::new();
        rt.add_repo(dir.path().to_path_buf()).await.unwrap();

        let basename = dir
            .path()
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap()
            .to_string();
        let args = WaitArgs {
            role: WaitingRole::Reviewers,
            plan_id: None,
            repo: Some(basename),
            author_label: Some("codex".to_string()),
            timeout_secs: Some(2),
        };
        let resp = wait_for_work(&rt, args).await.unwrap();
        let payload = match resp {
            WaitResponse::Work(p) => p,
            WaitResponse::Timeout(_) => panic!("expected work, got timeout"),
        };
        assert!(
            payload.work.plans.iter().any(|p| p.ends_with("first.md")),
            "fold-order walk must return `first` (the earlier commit), even \
             when both commits share the same author_ts; got plans={:?}",
            payload.work.plans
        );
    }

    /// Phase 3 regression: empty plan_id + empty repo must error
    /// (the matcher needs a scope).
    #[tokio::test]
    async fn missing_repo_with_empty_plan_id_errors() {
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
        commit(dir.path(), "add foo");
        let rt = Runtime::new();
        rt.add_repo(dir.path().to_path_buf()).await.unwrap();

        let no_scope_args = WaitArgs {
            role: WaitingRole::Reviewers,
            plan_id: None,
            repo: None,
            author_label: Some("codex".to_string()),
            timeout_secs: Some(2),
        };
        let err = wait_for_work(&rt, no_scope_args).await.unwrap_err();
        assert!(matches!(err, WaitError::MissingRepoForRepoScope));
    }

    #[tokio::test]
    async fn body_dirty_yields_commit_plan_revision_for_master() {
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo v1\n");
        commit(dir.path(), "add foo");
        let rt = Runtime::new();
        rt.add_repo(dir.path().to_path_buf()).await.unwrap();
        // Edit but don't commit.
        write_file(
            dir.path(),
            ".trinity/plans/foo.md",
            "# foo v2 uncommitted\n",
        );

        let resp = wait_for_work(&rt, args(dir.path(), WaitingRole::Master, "foo", "lloyd"))
            .await
            .unwrap();
        let (work, locations) = expect_work(resp);
        assert_eq!(work, "commit_plan_revision");
        assert_eq!(locations, vec![".trinity/plans/foo.md".to_string()]);
    }

    #[tokio::test]
    async fn address_plan_request_changes_lists_rc_then_plan_file() {
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
        commit(dir.path(), "add foo");
        let rt = Runtime::new();
        rt.add_repo(dir.path().to_path_buf()).await.unwrap();

        let intro: CommitSha = rt
            .read_repo(dir.path(), |s| {
                s.plans[&PlanKey::parse("foo").unwrap()].plan_intro.clone()
            })
            .await
            .unwrap();
        // Two RC feedbacks at the canonical path.
        let bob_path = format!(".trinity/feedback/foo/{}/bob.md", intro.as_str());
        let dana_path = format!(".trinity/feedback/foo/{}/dana.md", intro.as_str());
        write_file(dir.path(), &bob_path, "REQUEST_CHANGES\n");
        write_file(dir.path(), &dana_path, "REQUEST_CHANGES\n");
        rt.handle_signal(
            dir.path(),
            FilesystemSignal::FeedbackWritten {
                parsed: crate::disk_format::parse_feedback_path(&PathBuf::from(format!(
                    "foo/{}/bob.md",
                    intro.as_str()
                )))
                .unwrap(),
            },
            1,
        )
        .await
        .unwrap();
        rt.handle_signal(
            dir.path(),
            FilesystemSignal::FeedbackWritten {
                parsed: crate::disk_format::parse_feedback_path(&PathBuf::from(format!(
                    "foo/{}/dana.md",
                    intro.as_str()
                )))
                .unwrap(),
            },
            1,
        )
        .await
        .unwrap();

        let resp = wait_for_work(&rt, args(dir.path(), WaitingRole::Master, "foo", "lloyd"))
            .await
            .unwrap();
        let (work, locations) = expect_work(resp);
        assert_eq!(work, "address_changes");
        assert_eq!(
            locations,
            vec![bob_path, dana_path, ".trinity/plans/foo.md".to_string()]
        );
    }

    #[tokio::test]
    async fn caller_already_voted_does_not_re_wake_reviewer() {
        // Two participants on a revised plan target: codex has APPROVE'd
        // the current target, bob is still missing. The gate is
        // NeedsReview (waiting on reviewers) → `plan_needs_rereview`.
        // codex's wait_for_work must time out (their vote stands).
        // bob's wait_for_work must return review_plan.
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo v1\n");
        commit(dir.path(), "add foo v1");
        let rt = Runtime::new();
        rt.add_repo(dir.path().to_path_buf()).await.unwrap();

        let intro: CommitSha = rt
            .read_repo(dir.path(), |s| {
                s.plans[&PlanKey::parse("foo").unwrap()].plan_intro.clone()
            })
            .await
            .unwrap();
        // Both codex + bob approve the intro target → participants.
        for author in ["codex", "bob"] {
            let rel = format!(".trinity/feedback/foo/{}/{}.md", intro.as_str(), author);
            write_file(dir.path(), &rel, "APPROVE\n");
            let parsed_rel = PathBuf::from(format!("foo/{}/{}.md", intro.as_str(), author));
            let parsed = crate::disk_format::parse_feedback_path(&parsed_rel).unwrap();
            rt.handle_signal(dir.path(), FilesystemSignal::FeedbackWritten { parsed }, 1)
                .await
                .unwrap();
        }

        // Revise the plan so the target SHA advances; codex re-approves
        // the new target. bob stays a participant but hasn't re-voted.
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo v2\n");
        commit(dir.path(), "revise foo");
        rt.handle_signal(dir.path(), FilesystemSignal::HeadChanged, 2)
            .await
            .unwrap();
        let revised: CommitSha = rt
            .read_repo(dir.path(), |s| {
                let plan = &s.plans[&PlanKey::parse("foo").unwrap()];
                crate::projection::all_plan_revisions(plan, s)
                    .into_iter()
                    .next_back()
                    .unwrap()
            })
            .await
            .unwrap();
        let codex_rel = format!(".trinity/feedback/foo/{}/codex.md", revised.as_str());
        write_file(dir.path(), &codex_rel, "APPROVE\n");
        let parsed = crate::disk_format::parse_feedback_path(&PathBuf::from(format!(
            "foo/{}/codex.md",
            revised.as_str()
        )))
        .unwrap();
        rt.handle_signal(dir.path(), FilesystemSignal::FeedbackWritten { parsed }, 3)
            .await
            .unwrap();

        // codex polls → already voted → times out.
        let mut a = args(dir.path(), WaitingRole::Reviewers, "foo", "codex");
        a.timeout_secs = Some(1);
        let resp = wait_for_work(&rt, a).await.unwrap();
        expect_timeout(resp);

        // bob polls → still missing → returns review work.
        let mut a = args(dir.path(), WaitingRole::Reviewers, "foo", "bob");
        a.timeout_secs = Some(1);
        let resp = wait_for_work(&rt, a).await.unwrap();
        let (work, locations) = expect_work(resp);
        assert_eq!(work, "write_feedback");
        assert_eq!(locations.len(), 1);
        assert!(
            locations[0].ends_with("/bob.md"),
            "bob's write path should be returned, got {}",
            locations[0]
        );
    }

    #[tokio::test]
    async fn same_stem_different_repos_route_to_correct_repo() {
        // Two tempdir repos with distinct basenames + the same plan
        // stem. wait_for_work scoped to each repo's basename must
        // resolve to that repo's plan and return locations rooted
        // under it — never confused across repos.
        let parent_a = tempfile::tempdir().unwrap();
        let parent_b = tempfile::tempdir().unwrap();
        let dir_a = parent_a.path().join("alpha");
        let dir_b = parent_b.path().join("beta");
        std::fs::create_dir_all(&dir_a).unwrap();
        std::fs::create_dir_all(&dir_b).unwrap();
        for d in [&dir_a, &dir_b] {
            run_git(d, &["init", "--quiet", "--initial-branch=main"]);
            run_git(d, &["config", "user.email", "test@test"]);
            run_git(d, &["config", "user.name", "test"]);
            run_git(d, &["config", "commit.gpgsign", "false"]);
        }
        write_file(&dir_a, ".trinity/plans/shared.md", "# in alpha\n");
        commit(&dir_a, "alpha shared");
        write_file(&dir_b, ".trinity/plans/shared.md", "# in beta\n");
        commit(&dir_b, "beta shared");

        let rt = Runtime::new();
        rt.add_repo(dir_a.clone()).await.unwrap();
        rt.add_repo(dir_b.clone()).await.unwrap();

        let resp_a = wait_for_work(&rt, args(&dir_a, WaitingRole::Reviewers, "shared", "codex"))
            .await
            .unwrap();
        let resp_b = wait_for_work(&rt, args(&dir_b, WaitingRole::Reviewers, "shared", "codex"))
            .await
            .unwrap();
        match (resp_a, resp_b) {
            (WaitResponse::Work(p_a), WaitResponse::Work(p_b)) => {
                assert_eq!(p_a.work.plan_id, "alpha/shared.md");
                assert_eq!(p_b.work.plan_id, "beta/shared.md");
                assert_ne!(p_a.work.repo, p_b.work.repo);
                assert!(p_a.work.repo.ends_with("alpha"));
                assert!(p_b.work.repo.ends_with("beta"));
            }
            other => panic!("both calls should return Work; got {other:?}"),
        }
    }

    #[tokio::test]
    async fn timeout_returns_timed_out_shape() {
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
        commit(dir.path(), "add foo");
        let rt = Runtime::new();
        rt.add_repo(dir.path().to_path_buf()).await.unwrap();

        // Polling master while only reviewers have work — should time out.
        let mut a = args(dir.path(), WaitingRole::Master, "foo", "lloyd");
        a.timeout_secs = Some(1);
        let resp = wait_for_work(&rt, a).await.unwrap();
        expect_timeout(resp);
    }

    /// Cross-surface invariant: for the same `(snapshot, author)`,
    /// `wait_for_work` and `work_context` MUST produce the same
    /// `WorkPayload`. Both surfaces call
    /// `responses::build_work_payload`; this test guards against any
    /// future regression that adds a second projection path.
    #[tokio::test]
    async fn wait_for_work_and_work_context_agree_on_work_payload() {
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo v1\n");
        commit(dir.path(), "add foo");
        let rt = Runtime::new();
        rt.add_repo(dir.path().to_path_buf()).await.unwrap();

        // wait_for_work as reviewer — the initial-review state.
        let mut a = args(dir.path(), WaitingRole::Reviewers, "foo", "codex");
        a.timeout_secs = Some(1);
        let wfw = wait_for_work(&rt, a).await.unwrap();
        let wfw_payload = match wfw {
            WaitResponse::Work(p) => p,
            WaitResponse::Timeout(_) => panic!("expected work, got timeout"),
        };

        // work_context for the same plan / author. It carries the
        // same `WorkPayload` embedded as `work`.
        let snapshot = rt
            .snapshot_session(dir.path(), &PlanKey::parse("foo").unwrap())
            .await
            .unwrap()
            .expect("foo snapshot");
        let author = AgentLabel::parse("codex").unwrap();
        let wc = crate::responses::work_context_response(&snapshot, &author)
            .unwrap()
            .expect("foo is visible");

        // The projection (`build_work_payload`) is shared, so
        // the underlying `WorkPayload` is identical. WFW's
        // wait-only enrichment populates `plan_file.content`
        // / `review.content` opportunistically; for the
        // cross-surface invariant we strip those before
        // comparing.
        let mut wfw_work = wfw_payload.work.clone();
        strip_inline_content(&mut wfw_work.action);
        assert_eq!(wfw_work, wc.work);
    }

    /// Drop opportunistic inline `content` so a content-aware
    /// payload can be equality-compared with a content-stripped
    /// (e.g. work_context) one. Mutates in place.
    fn strip_inline_content(a: &mut trinity_core::api::ExpectedAction) {
        use trinity_core::api::ExpectedAction::*;
        match a {
            WriteFeedback { plan_file, .. } => plan_file.content = None,
            AddressChanges { reviews, .. } => {
                for r in reviews.iter_mut() {
                    r.content = None;
                }
            }
            CommitPlanRevision { .. } | StartImplementation { .. } | SessionFinished => {}
        }
    }

    fn poll_reviewer(dir: &std::path::Path) -> WaitArgs {
        let mut a = args(dir, WaitingRole::Reviewers, "foo", "codex");
        a.timeout_secs = Some(1);
        a
    }

    /// First poll for a reviewer carries `plan_file.content`; second
    /// poll for the same agent omits it (cache hit).
    #[tokio::test]
    async fn write_feedback_plan_file_content_opportunistic() {
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo plan v1\n");
        commit(dir.path(), "add foo");
        let rt = Runtime::new();
        rt.add_repo(dir.path().to_path_buf()).await.unwrap();

        let resp = wait_for_work(&rt, poll_reviewer(dir.path())).await.unwrap();
        let p1 = match resp {
            WaitResponse::Work(p) => p,
            _ => panic!("expected work"),
        };
        match &p1.work.action {
            trinity_core::api::ExpectedAction::WriteFeedback { plan_file, .. } => {
                assert_eq!(
                    plan_file.content.as_deref(),
                    Some("# foo plan v1\n"),
                    "first poll should carry plan_file.content"
                );
            }
            other => panic!("expected WriteFeedback, got {other:?}"),
        }

        let resp2 = wait_for_work(&rt, poll_reviewer(dir.path())).await.unwrap();
        let p2 = match resp2 {
            WaitResponse::Work(p) => p,
            _ => panic!("expected work"),
        };
        match &p2.work.action {
            trinity_core::api::ExpectedAction::WriteFeedback { plan_file, .. } => {
                assert!(
                    plan_file.content.is_none(),
                    "second poll should omit plan_file.content; got: {plan_file:?}"
                );
            }
            other => panic!("expected WriteFeedback, got {other:?}"),
        }
    }

    /// Plan body changes → hash changes → next poll re-sends content.
    #[tokio::test]
    async fn write_feedback_content_resends_on_hash_change() {
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# v1\n");
        commit(dir.path(), "add foo");
        let rt = Runtime::new();
        rt.add_repo(dir.path().to_path_buf()).await.unwrap();

        let _ = wait_for_work(&rt, poll_reviewer(dir.path())).await.unwrap();
        write_file(dir.path(), ".trinity/plans/foo.md", "# v2\n");
        commit(dir.path(), "revise foo");
        rt.handle_signal(dir.path(), FilesystemSignal::HeadChanged, 1)
            .await
            .unwrap();
        let resp = wait_for_work(&rt, poll_reviewer(dir.path())).await.unwrap();
        let p = match resp {
            WaitResponse::Work(p) => p,
            _ => panic!("expected work"),
        };
        match &p.work.action {
            trinity_core::api::ExpectedAction::WriteFeedback { plan_file, .. } => {
                assert_eq!(
                    plan_file.content.as_deref(),
                    Some("# v2\n"),
                    "hash mismatch should re-send content"
                );
            }
            other => panic!("expected WriteFeedback, got {other:?}"),
        }
    }

    /// Files larger than `MAX_INLINE_BODY` get `content: None`,
    /// but the cache is marked anyway so we don't re-read on every
    /// poll. Second poll re-reads the file but short-circuits on
    /// cache hit before considering the cap.
    #[tokio::test]
    async fn write_feedback_oversize_skips_content_and_marks_seen() {
        let dir = init_repo();
        let oversize = "x".repeat(super::MAX_INLINE_BODY + 1);
        write_file(dir.path(), ".trinity/plans/foo.md", &oversize);
        commit(dir.path(), "add foo");
        let rt = Runtime::new();
        rt.add_repo(dir.path().to_path_buf()).await.unwrap();

        let resp = wait_for_work(&rt, poll_reviewer(dir.path())).await.unwrap();
        let p1 = match resp {
            WaitResponse::Work(p) => p,
            _ => panic!("expected work"),
        };
        match &p1.work.action {
            trinity_core::api::ExpectedAction::WriteFeedback { plan_file, .. } => {
                assert!(
                    plan_file.content.is_none(),
                    "oversize file should omit content"
                );
            }
            other => panic!("expected WriteFeedback, got {other:?}"),
        }

        let author = AgentLabel::parse("codex").unwrap();
        let canonical = dunce::canonicalize(dir.path()).unwrap();
        let hash = crate::lifecycle::content_hash(&oversize);
        assert!(
            rt.opportunistic_body_seen(&canonical, &author, ".trinity/plans/foo.md", &hash)
                .await,
            "oversize body should still be marked seen so we don't re-read on every poll"
        );
    }

    /// Master polling sees an RC on a superseded commit exactly
    /// once in `stale_reviews`; the next poll has an empty sidecar.
    /// Reviewer polling never receives the sidecar.
    #[tokio::test]
    async fn master_gets_stale_review_once_then_suppressed() {
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo v1\n");
        commit(dir.path(), "add foo");
        let rt = Runtime::new();
        rt.add_repo(dir.path().to_path_buf()).await.unwrap();

        let intro: CommitSha = rt
            .read_repo(dir.path(), |s| {
                s.plans[&PlanKey::parse("foo").unwrap()].plan_intro.clone()
            })
            .await
            .unwrap();
        let stale_path = format!(".trinity/feedback/foo/{}/codex.md", intro.as_str());
        write_file(dir.path(), &stale_path, "REQUEST_CHANGES\nfix it\n");
        rt.handle_signal(
            dir.path(),
            FilesystemSignal::FeedbackWritten {
                parsed: crate::disk_format::parse_feedback_path(&PathBuf::from(format!(
                    "foo/{}/codex.md",
                    intro.as_str()
                )))
                .unwrap(),
            },
            1,
        )
        .await
        .unwrap();

        write_file(dir.path(), ".trinity/plans/foo.md", "# foo v2\n");
        commit(dir.path(), "revise foo");
        rt.handle_signal(dir.path(), FilesystemSignal::HeadChanged, 2)
            .await
            .unwrap();
        let new_head: CommitSha = rt
            .read_repo(dir.path(), |s| s.head.clone().expect("head"))
            .await
            .unwrap();
        let current_path = format!(".trinity/feedback/foo/{}/codex.md", new_head.as_str());
        write_file(dir.path(), &current_path, "REQUEST_CHANGES\nstill broken\n");
        rt.handle_signal(
            dir.path(),
            FilesystemSignal::FeedbackWritten {
                parsed: crate::disk_format::parse_feedback_path(&PathBuf::from(format!(
                    "foo/{}/codex.md",
                    new_head.as_str()
                )))
                .unwrap(),
            },
            3,
        )
        .await
        .unwrap();

        let mut a = args(dir.path(), WaitingRole::Master, "foo", "lloyd");
        a.timeout_secs = Some(1);
        let resp = wait_for_work(&rt, a).await.unwrap();
        let p1 = match resp {
            WaitResponse::Work(p) => p,
            _ => panic!("expected work"),
        };
        assert_eq!(p1.stale_reviews.len(), 1, "first poll should carry stale");
        let stale = &p1.stale_reviews[0];
        assert_eq!(stale.path, stale_path);
        assert_eq!(stale.target_sha, intro.as_str());
        assert_eq!(stale.author.as_str(), "codex");
        assert_eq!(
            stale.content.as_deref(),
            Some("REQUEST_CHANGES\nfix it\n"),
            "stale review content should be inlined on first delivery"
        );

        let mut a2 = args(dir.path(), WaitingRole::Master, "foo", "lloyd");
        a2.timeout_secs = Some(1);
        let resp2 = wait_for_work(&rt, a2).await.unwrap();
        let p2 = match resp2 {
            WaitResponse::Work(p) => p,
            _ => panic!("expected work"),
        };
        assert!(
            p2.stale_reviews.is_empty(),
            "second poll should suppress stale review; got: {:?}",
            p2.stale_reviews
        );
    }

    /// Master `AddressChanges`: first poll inlines per-review content
    /// (mixed RC + Unmarked rows), second poll omits same-hash content.
    /// Locks down both the opportunistic-fill behavior for reviews[]
    /// and the verdict preservation (Unmarked must surface, not be
    /// silently coerced to RequestChanges).
    #[tokio::test]
    async fn address_changes_reviews_content_opportunistic_with_mixed_verdicts() {
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
        commit(dir.path(), "add foo");
        let rt = Runtime::new();
        rt.add_repo(dir.path().to_path_buf()).await.unwrap();

        let intro: CommitSha = rt
            .read_repo(dir.path(), |s| {
                s.plans[&PlanKey::parse("foo").unwrap()].plan_intro.clone()
            })
            .await
            .unwrap();
        let bob_path = format!(".trinity/feedback/foo/{}/bob.md", intro.as_str());
        let dana_path = format!(".trinity/feedback/foo/{}/dana.md", intro.as_str());
        write_file(dir.path(), &bob_path, "REQUEST_CHANGES\nbob says fix\n");
        write_file(dir.path(), &dana_path, "no verdict marker here\n");
        rt.handle_signal(
            dir.path(),
            FilesystemSignal::FeedbackWritten {
                parsed: crate::disk_format::parse_feedback_path(&PathBuf::from(format!(
                    "foo/{}/bob.md",
                    intro.as_str()
                )))
                .unwrap(),
            },
            1,
        )
        .await
        .unwrap();
        rt.handle_signal(
            dir.path(),
            FilesystemSignal::FeedbackWritten {
                parsed: crate::disk_format::parse_feedback_path(&PathBuf::from(format!(
                    "foo/{}/dana.md",
                    intro.as_str()
                )))
                .unwrap(),
            },
            1,
        )
        .await
        .unwrap();

        let mut a1 = args(dir.path(), WaitingRole::Master, "foo", "lloyd");
        a1.timeout_secs = Some(1);
        let resp = wait_for_work(&rt, a1).await.unwrap();
        let p1 = match resp {
            WaitResponse::Work(p) => p,
            _ => panic!("expected work"),
        };
        match &p1.work.action {
            trinity_core::api::ExpectedAction::AddressChanges { reviews, .. } => {
                assert_eq!(reviews.len(), 2);
                let bob = reviews
                    .iter()
                    .find(|r| r.author.as_str() == "bob")
                    .expect("bob");
                assert_eq!(bob.verdict, trinity_core::Verdict::RequestChanges);
                assert_eq!(
                    bob.content.as_deref(),
                    Some("REQUEST_CHANGES\nbob says fix\n"),
                    "RC content must be inlined on first poll"
                );
                let dana = reviews
                    .iter()
                    .find(|r| r.author.as_str() == "dana")
                    .expect("dana");
                assert_eq!(
                    dana.verdict,
                    trinity_core::Verdict::Unmarked,
                    "Unmarked verdict must surface (not be coerced)"
                );
                assert_eq!(
                    dana.content.as_deref(),
                    Some("no verdict marker here\n"),
                    "Unmarked content must be inlined on first poll"
                );
            }
            other => panic!("expected AddressChanges, got {other:?}"),
        }

        let mut a2 = args(dir.path(), WaitingRole::Master, "foo", "lloyd");
        a2.timeout_secs = Some(1);
        let resp2 = wait_for_work(&rt, a2).await.unwrap();
        let p2 = match resp2 {
            WaitResponse::Work(p) => p,
            _ => panic!("expected work"),
        };
        match &p2.work.action {
            trinity_core::api::ExpectedAction::AddressChanges { reviews, .. } => {
                for r in reviews {
                    assert!(
                        r.content.is_none(),
                        "second poll must omit content for {} (cache hit); got {:?}",
                        r.author.as_str(),
                        r.content
                    );
                }
            }
            other => panic!("expected AddressChanges, got {other:?}"),
        }
    }

    /// Path-keyed semantics: editing a stale feedback file between
    /// polls must NOT re-surface it. Stale reviews are one-shot per
    /// `(repo, agent, path)`; only `opportunistic_bodies` is
    /// hash-keyed.
    #[tokio::test]
    async fn stale_review_path_keyed_not_hash_keyed() {
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo v1\n");
        commit(dir.path(), "add foo");
        let rt = Runtime::new();
        rt.add_repo(dir.path().to_path_buf()).await.unwrap();

        let intro: CommitSha = rt
            .read_repo(dir.path(), |s| {
                s.plans[&PlanKey::parse("foo").unwrap()].plan_intro.clone()
            })
            .await
            .unwrap();
        let stale_path = format!(".trinity/feedback/foo/{}/codex.md", intro.as_str());
        write_file(dir.path(), &stale_path, "REQUEST_CHANGES\nv1\n");
        rt.handle_signal(
            dir.path(),
            FilesystemSignal::FeedbackWritten {
                parsed: crate::disk_format::parse_feedback_path(&PathBuf::from(format!(
                    "foo/{}/codex.md",
                    intro.as_str()
                )))
                .unwrap(),
            },
            1,
        )
        .await
        .unwrap();

        write_file(dir.path(), ".trinity/plans/foo.md", "# foo v2\n");
        commit(dir.path(), "revise foo");
        rt.handle_signal(dir.path(), FilesystemSignal::HeadChanged, 2)
            .await
            .unwrap();
        let head: CommitSha = rt
            .read_repo(dir.path(), |s| s.head.clone().expect("head"))
            .await
            .unwrap();
        let current_path = format!(".trinity/feedback/foo/{}/codex.md", head.as_str());
        write_file(dir.path(), &current_path, "REQUEST_CHANGES\nstill\n");
        rt.handle_signal(
            dir.path(),
            FilesystemSignal::FeedbackWritten {
                parsed: crate::disk_format::parse_feedback_path(&PathBuf::from(format!(
                    "foo/{}/codex.md",
                    head.as_str()
                )))
                .unwrap(),
            },
            3,
        )
        .await
        .unwrap();

        let mut a = args(dir.path(), WaitingRole::Master, "foo", "lloyd");
        a.timeout_secs = Some(1);
        let resp = wait_for_work(&rt, a).await.unwrap();
        let p1 = match resp {
            WaitResponse::Work(p) => p,
            _ => panic!("expected work"),
        };
        assert_eq!(p1.stale_reviews.len(), 1);

        write_file(dir.path(), &stale_path, "REQUEST_CHANGES\nv1 rewritten\n");

        let mut a2 = args(dir.path(), WaitingRole::Master, "foo", "lloyd");
        a2.timeout_secs = Some(1);
        let resp2 = wait_for_work(&rt, a2).await.unwrap();
        let p2 = match resp2 {
            WaitResponse::Work(p) => p,
            _ => panic!("expected work"),
        };
        assert!(
            p2.stale_reviews.is_empty(),
            "edited stale feedback must NOT re-surface (path-keyed cache); got: {:?}",
            p2.stale_reviews
        );
    }

    /// Three-commit timeline with feedback on two superseded shas
    /// and the third sha current. The sidecar carries exactly the
    /// two stale entries; current-target feedback rides on
    /// `reviews[]`, not on `stale_reviews`.
    #[tokio::test]
    async fn stale_reviews_multi_commit_timeline() {
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# v1\n");
        commit(dir.path(), "v1");
        let rt = Runtime::new();
        rt.add_repo(dir.path().to_path_buf()).await.unwrap();

        let sha1: CommitSha = rt
            .read_repo(dir.path(), |s| {
                s.plans[&PlanKey::parse("foo").unwrap()].plan_intro.clone()
            })
            .await
            .unwrap();
        let p1_fb = format!(".trinity/feedback/foo/{}/codex.md", sha1.as_str());
        write_file(dir.path(), &p1_fb, "REQUEST_CHANGES\non v1\n");
        rt.handle_signal(
            dir.path(),
            FilesystemSignal::FeedbackWritten {
                parsed: crate::disk_format::parse_feedback_path(&PathBuf::from(format!(
                    "foo/{}/codex.md",
                    sha1.as_str()
                )))
                .unwrap(),
            },
            1,
        )
        .await
        .unwrap();

        write_file(dir.path(), ".trinity/plans/foo.md", "# v2\n");
        commit(dir.path(), "v2");
        rt.handle_signal(dir.path(), FilesystemSignal::HeadChanged, 2)
            .await
            .unwrap();
        let sha2: CommitSha = rt
            .read_repo(dir.path(), |s| s.head.clone().expect("head"))
            .await
            .unwrap();
        let p2_fb = format!(".trinity/feedback/foo/{}/codex.md", sha2.as_str());
        write_file(dir.path(), &p2_fb, "REQUEST_CHANGES\non v2\n");
        rt.handle_signal(
            dir.path(),
            FilesystemSignal::FeedbackWritten {
                parsed: crate::disk_format::parse_feedback_path(&PathBuf::from(format!(
                    "foo/{}/codex.md",
                    sha2.as_str()
                )))
                .unwrap(),
            },
            3,
        )
        .await
        .unwrap();

        write_file(dir.path(), ".trinity/plans/foo.md", "# v3\n");
        commit(dir.path(), "v3");
        rt.handle_signal(dir.path(), FilesystemSignal::HeadChanged, 4)
            .await
            .unwrap();
        let sha3: CommitSha = rt
            .read_repo(dir.path(), |s| s.head.clone().expect("head"))
            .await
            .unwrap();
        let p3_fb = format!(".trinity/feedback/foo/{}/codex.md", sha3.as_str());
        write_file(dir.path(), &p3_fb, "REQUEST_CHANGES\non v3\n");
        rt.handle_signal(
            dir.path(),
            FilesystemSignal::FeedbackWritten {
                parsed: crate::disk_format::parse_feedback_path(&PathBuf::from(format!(
                    "foo/{}/codex.md",
                    sha3.as_str()
                )))
                .unwrap(),
            },
            5,
        )
        .await
        .unwrap();

        let mut a = args(dir.path(), WaitingRole::Master, "foo", "lloyd");
        a.timeout_secs = Some(1);
        let resp = wait_for_work(&rt, a).await.unwrap();
        let p = match resp {
            WaitResponse::Work(p) => p,
            _ => panic!("expected work"),
        };
        let mut got: Vec<(&str, &str)> = p
            .stale_reviews
            .iter()
            .map(|s| (s.path.as_str(), s.target_sha.as_str()))
            .collect();
        got.sort();
        let mut want = vec![
            (p1_fb.as_str(), sha1.as_str()),
            (p2_fb.as_str(), sha2.as_str()),
        ];
        want.sort();
        assert_eq!(got, want, "expected exactly two stale entries (v1, v2)");

        match &p.work.action {
            trinity_core::api::ExpectedAction::AddressChanges {
                reviews,
                target_sha,
                ..
            } => {
                assert_eq!(target_sha, sha3.as_str());
                assert_eq!(reviews.len(), 1);
                assert_eq!(reviews[0].path, p3_fb);
                assert!(
                    !p.stale_reviews.iter().any(|s| s.path == p3_fb),
                    "current-target feedback must not appear in stale_reviews"
                );
            }
            other => panic!("expected AddressChanges, got {other:?}"),
        }
    }

    /// `work_context` never populates `plan_file.content`.
    #[tokio::test]
    async fn work_context_omits_opportunistic_content() {
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
        commit(dir.path(), "add foo");
        let rt = Runtime::new();
        rt.add_repo(dir.path().to_path_buf()).await.unwrap();

        let snapshot = rt
            .snapshot_session(dir.path(), &PlanKey::parse("foo").unwrap())
            .await
            .unwrap()
            .expect("foo snapshot");
        let author = AgentLabel::parse("codex").unwrap();
        let wc = crate::responses::work_context_response(&snapshot, &author)
            .unwrap()
            .expect("foo is visible");

        match &wc.work.action {
            trinity_core::api::ExpectedAction::WriteFeedback { plan_file, .. } => {
                assert!(
                    plan_file.content.is_none(),
                    "work_context must not populate plan_file.content"
                );
            }
            other => panic!("expected WriteFeedback, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn wakes_on_plan_file_changed_event() {
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo v1\n");
        commit(dir.path(), "add foo");
        let rt = std::sync::Arc::new(Runtime::new());
        rt.add_repo(dir.path().to_path_buf()).await.unwrap();

        let rt2 = std::sync::Arc::clone(&rt);
        let repo = dir.path().to_path_buf();
        let join = tokio::spawn(async move {
            let mut a = args(&repo, WaitingRole::Master, "foo", "lloyd");
            a.timeout_secs = Some(5);
            wait_for_work(&rt2, a).await
        });

        tokio::time::sleep(Duration::from_millis(200)).await;
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo v2 unsaved\n");
        rt.handle_signal(
            dir.path(),
            FilesystemSignal::PlanFileChanged {
                session_id: PlanKey::parse("foo").unwrap(),
                path: PathBuf::from(".trinity/plans/foo.md"),
            },
            42,
        )
        .await
        .unwrap();

        let resp = tokio::time::timeout(Duration::from_secs(3), join)
            .await
            .expect("wait_for_work didn't return")
            .unwrap()
            .unwrap();
        let (work, _) = expect_work(resp);
        assert_eq!(work, "commit_plan_revision");
    }

    #[tokio::test]
    async fn unknown_session_errors() {
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
        commit(dir.path(), "add foo");
        let rt = Runtime::new();
        rt.add_repo(dir.path().to_path_buf()).await.unwrap();

        let a = args(
            dir.path(),
            WaitingRole::Reviewers,
            "does-not-exist",
            "codex",
        );
        let err = wait_for_work(&rt, a).await.unwrap_err();
        assert!(matches!(err, WaitError::UnknownPlan(_)));
    }

    #[tokio::test]
    async fn unknown_repo_errors() {
        let rt = Runtime::new();
        let a = WaitArgs {
            role: WaitingRole::Reviewers,
            plan_id: Some("no-such-repo/foo.md".to_string()),
            author_label: Some("codex".to_string()),
            timeout_secs: Some(1),
            repo: None,
        };
        let err = wait_for_work(&rt, a).await.unwrap_err();
        assert!(matches!(err, WaitError::UnknownRepo(_)));
    }

    /// `WaitingRole::None` is the projected role for finalized plans
    /// (terminal state, nobody is blocking). The role-gate exception
    /// in `compute_match` lets `SessionFinished` through regardless
    /// of the caller's requested role. Without it, a long-poll caller
    /// blocked before the Finalize commit lands would sleep through
    /// the broadcast wakeup and time out — the exact bug this plan
    /// fixes.
    ///
    /// Master and reviewer cases need different setups: master blocks
    /// naturally on `add foo` (waiting_on=reviewers); a reviewer would
    /// get review_commit immediately, so we pre-approve from `bob` so
    /// waiting_on flips to `{role:Master, reason:ReadyToStartImpl}`
    /// and `bob`'s wait_for_work blocks on role-mismatch.

    #[tokio::test]
    async fn finalize_wakes_blocked_master() {
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
        commit(dir.path(), "add foo");
        let rt = std::sync::Arc::new(Runtime::new());
        rt.add_repo(dir.path().to_path_buf()).await.unwrap();

        let rt2 = std::sync::Arc::clone(&rt);
        let repo = dir.path().to_path_buf();
        let join = tokio::spawn(async move {
            let mut a = args(&repo, WaitingRole::Master, "foo", "lloyd");
            a.timeout_secs = Some(5);
            wait_for_work(&rt2, a).await
        });

        tokio::time::sleep(Duration::from_millis(200)).await;
        write_file(
            dir.path(),
            ".trinity/finished/foo/codex.md",
            "APPROVE\n\nlgtm\n",
        );
        commit(dir.path(), "Finalize foo");
        rt.handle_signal(dir.path(), FilesystemSignal::HeadChanged, 1)
            .await
            .unwrap();

        let resp = tokio::time::timeout(Duration::from_secs(3), join)
            .await
            .expect("wait_for_work didn't return after finalize")
            .unwrap()
            .unwrap();
        let (work, locations) = expect_work(resp);
        assert_eq!(work, "session_finished");
        assert!(locations.is_empty());
    }

    #[tokio::test]
    async fn finalize_wakes_blocked_reviewer() {
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
        commit(dir.path(), "add foo");
        let rt = std::sync::Arc::new(Runtime::new());
        rt.add_repo(dir.path().to_path_buf()).await.unwrap();

        // bob approves the intro, flipping waiting_on to master-side
        // so bob's reviewer wait_for_work blocks on role-mismatch.
        let intro: CommitSha = rt
            .read_repo(dir.path(), |s| {
                s.plans[&PlanKey::parse("foo").unwrap()].plan_intro.clone()
            })
            .await
            .unwrap();
        let rel = format!(".trinity/feedback/foo/{}/bob.md", intro.as_str());
        write_file(dir.path(), &rel, "APPROVE\n");
        let parsed = crate::disk_format::parse_feedback_path(&PathBuf::from(format!(
            "foo/{}/bob.md",
            intro.as_str()
        )))
        .unwrap();
        rt.handle_signal(dir.path(), FilesystemSignal::FeedbackWritten { parsed }, 1)
            .await
            .unwrap();

        let rt2 = std::sync::Arc::clone(&rt);
        let repo = dir.path().to_path_buf();
        let join = tokio::spawn(async move {
            let mut a = args(&repo, WaitingRole::Reviewers, "foo", "bob");
            a.timeout_secs = Some(5);
            wait_for_work(&rt2, a).await
        });

        tokio::time::sleep(Duration::from_millis(200)).await;
        write_file(
            dir.path(),
            ".trinity/finished/foo/codex.md",
            "APPROVE\n\nlgtm\n",
        );
        commit(dir.path(), "Finalize foo");
        rt.handle_signal(dir.path(), FilesystemSignal::HeadChanged, 2)
            .await
            .unwrap();

        let resp = tokio::time::timeout(Duration::from_secs(3), join)
            .await
            .expect("wait_for_work didn't return after finalize")
            .unwrap()
            .unwrap();
        let (work, locations) = expect_work(resp);
        assert_eq!(work, "session_finished");
        assert!(locations.is_empty());
    }

    #[tokio::test]
    async fn missing_author_label_errors_when_none() {
        let rt = Runtime::new();
        let a = WaitArgs {
            role: WaitingRole::Reviewers,
            plan_id: Some("anywhere/foo.md".to_string()),
            author_label: None,
            timeout_secs: Some(1),
            repo: None,
        };
        let err = wait_for_work(&rt, a).await.unwrap_err();
        assert!(matches!(err, WaitError::MissingAuthorLabel));
    }

    #[tokio::test]
    async fn missing_author_label_errors_when_blank() {
        let rt = Runtime::new();
        let a = WaitArgs {
            role: WaitingRole::Reviewers,
            plan_id: Some("anywhere/foo.md".to_string()),
            author_label: Some("   ".to_string()),
            timeout_secs: Some(1),
            repo: None,
        };
        let err = wait_for_work(&rt, a).await.unwrap_err();
        assert!(matches!(err, WaitError::MissingAuthorLabel));
    }
}
