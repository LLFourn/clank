//! In-process tests for `clank pr-review` phase 2 (local state +
//! verbs). Drive the cores directly — no binary spawning; git is
//! spawned for the PR-head fetch (allowed).

use crate::common;

use common::TestEnv;
use std::path::Path;
use std::process::Command;

use clank::cli::pr_review::{
    abort_with, draft_watch_belongs_to, note_with, propose_with, start_with, status_with,
};
use clank_core::ids::AgentLabel;
use clank_core::pr_review::ReviewerVerdict;
use clank_core::vocab::Verdict;

fn git(repo: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .status()
        .expect("git");
    assert!(status.success(), "git {args:?} failed");
}

fn git_out(repo: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .expect("git");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn label(s: &str) -> AgentLabel {
    AgentLabel::parse(s).unwrap()
}

/// Base commit + an `origin` bare remote carrying
/// `refs/pull/<pr>/head` so `start_with`'s fetch works. No team —
/// callers add one when they need it.
fn setup_repo_with_pr(env: &TestEnv, pr: u32) -> String {
    let repo = env.repo();
    std::fs::write(repo.join("file.rs"), "// base\n").unwrap();
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "base"]);

    // github.com-shaped path so `repo_slug` parses `LLFourn/clank`
    // from origin (the `--checkout` path needs it) while the fetch
    // still works offline.
    let bare = env.home().join("github.com/LLFourn/clank.git");
    std::fs::create_dir_all(bare.parent().unwrap()).unwrap();
    git(repo, &["init", "--bare", "--quiet", bare.to_str().unwrap()]);
    git(repo, &["remote", "add", "origin", bare.to_str().unwrap()]);
    git(repo, &["checkout", "--quiet", "-b", "pr-branch"]);
    std::fs::write(repo.join("file.rs"), "// pr change\n").unwrap();
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "pr change"]);
    let pr_sha = git_out(repo, &["rev-parse", "HEAD"]);
    git(
        repo,
        &[
            "push",
            "--quiet",
            "origin",
            &format!("HEAD:refs/pull/{pr}/head"),
        ],
    );
    git(repo, &["checkout", "--quiet", "-"]);
    git(repo, &["branch", "--quiet", "-D", "pr-branch"]);
    pr_sha
}

/// A repo with a base commit, a team, and an `origin` bare remote
/// carrying `refs/pull/<pr>/head` so `start_with`'s fetch works.
fn env_with_pr(pr: u32) -> (TestEnv, String) {
    let env = TestEnv::init();
    env.register_team("claude", &["codex"], &["ruthless"]);
    let pr_sha = setup_repo_with_pr(&env, pr);
    (env, pr_sha)
}

#[test]
fn start_scaffolds_and_pins_head() {
    let (env, pr_sha) = env_with_pr(123);
    let repo = env.repo();
    // pr-review must NOT manage the gitignore — seed the canonical allow-list
    // and assert `start` leaves it byte-identical (`.clank/pr-reviews/` is
    // already covered by `/*`).
    let gi_path = repo.join(".clank/.gitignore");
    std::fs::write(&gi_path, clank::init_facts::clank_gitignore_body()).unwrap();
    let gi_before = std::fs::read_to_string(&gi_path).unwrap();
    let dir = start_with(repo, "LLFourn/clank", 123, None, None).unwrap();

    assert!(dir.ends_with(".clank/pr-reviews/123"));
    let state: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("pr.json")).unwrap()).unwrap();
    assert_eq!(state["repo"], "LLFourn/clank");
    assert_eq!(state["number"], 123);
    assert_eq!(state["head_sha"], pr_sha, "head pinned to the PR head");
    assert_eq!(state["round"], 0);
    // The id is resolved on demand, never persisted: the key must be
    // ABSENT (is_null() would also pass for a present null, proving
    // nothing — codex c3ad98e).
    assert!(
        state.get("review_id").is_none(),
        "review id must not be persisted: {state}"
    );
    assert!(dir.join("master.md").is_file());
    assert!(dir.join("reviews").is_dir());

    // pr-review left the allow-list gitignore untouched (no per-dir append).
    let gi_after = std::fs::read_to_string(&gi_path).unwrap();
    assert_eq!(
        gi_after, gi_before,
        "pr-review must not touch the gitignore"
    );
}

#[test]
fn start_refuses_existing() {
    let (env, _) = env_with_pr(7);
    let repo = env.repo();
    start_with(repo, "o/r", 7, None, None).unwrap();
    let err = start_with(repo, "o/r", 7, None, None)
        .unwrap_err()
        .to_string();
    assert!(err.contains("already started"), "got: {err}");
}

#[test]
fn note_records_verdict_at_current_round() {
    let (env, _) = env_with_pr(123);
    let repo = env.repo();
    start_with(repo, "o/r", 123, None, None).unwrap();

    let path = note_with(
        repo,
        &label("codex"),
        Some(123),
        Verdict::Finished,
        "lgtm, ship it",
    )
    .unwrap();
    assert!(path.ends_with("reviews/codex.md"));
    let parsed = ReviewerVerdict::parse(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(parsed.verdict, Verdict::Finished);
    assert_eq!(parsed.reviewed_round, 0, "stamped with the current round");
    assert_eq!(parsed.summary, "lgtm, ship it");
}

#[test]
fn note_infers_single_active_pr() {
    let (env, _) = env_with_pr(42);
    let repo = env.repo();
    start_with(repo, "o/r", 42, None, None).unwrap();
    // No --pr: resolves to the single active review.
    let path = note_with(
        repo,
        &label("codex"),
        None,
        Verdict::RequestChanges,
        "needs work",
    )
    .unwrap();
    assert!(path.ends_with("reviews/codex.md"));
}

#[test]
fn status_reports_pending_then_converged() {
    let (env, _) = env_with_pr(123);
    let repo = env.repo();
    start_with(repo, "o/r", 123, None, None).unwrap();

    // Round 0: master drafting — no reviewers summoned yet.
    let s = status_with(repo, Some(env.home()), Some(123)).unwrap();
    assert!(s.contains("round 0"), "{s}");
    assert!(s.contains("drafting"), "{s}");
    assert!(
        !s.contains("waiting on:"),
        "no reviewers before propose: {s}"
    );

    // Master opens the review → reviewers summoned at round 1.
    propose_with(repo, Some(env.home()), &label("claude"), Some(123)).unwrap();
    let s = status_with(repo, Some(env.home()), Some(123)).unwrap();
    assert!(s.contains("round 1"), "{s}");
    assert!(s.contains("waiting on:"), "{s}");
    assert!(s.contains("codex") && s.contains("ruthless"), "{s}");

    note_with(repo, &label("codex"), Some(123), Verdict::Finished, "ok").unwrap();
    note_with(repo, &label("ruthless"), Some(123), Verdict::Finished, "ok").unwrap();
    let s = status_with(repo, Some(env.home()), Some(123)).unwrap();
    assert!(s.contains("converged"), "{s}");
}

#[test]
fn the_observers_memory_dies_with_the_review_that_owned_it() {
    let (env, _) = env_with_pr(123);
    let repo = env.repo();
    start_with(repo, "o/r", 123, None, None).unwrap();

    // Anchored by `start`; what is under test is WHERE it lives.
    let horizon = draft_horizon(repo, 123);
    let anchored = std::fs::read_to_string(&horizon).unwrap();

    propose_with(repo, Some(env.home()), &label("claude"), Some(123)).unwrap();
    assert_eq!(
        std::fs::read_to_string(&horizon).unwrap(),
        anchored,
        "a round boundary is not authorship: proposing must not touch what the watch has seen"
    );

    abort_with(repo, Some(env.home()), &label("claude"), Some(123)).unwrap();
    assert!(!horizon.exists());
}

/// The review's own draft horizon file. Named for the review RUN, so
/// two runs of one PR cannot share it.
fn draft_horizon(repo: &std::path::Path, pr: u32) -> std::path::PathBuf {
    let dir = repo.join(".clank/pr-reviews").join(pr.to_string());
    let mut found: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("draft-") && n.ends_with(".json"))
        })
        .collect();
    assert_eq!(found.len(), 1, "exactly one run's horizon in {dir:?}");
    found.pop().unwrap()
}

/// The github watches in master's config.
fn watches(repo: &std::path::Path) -> Vec<clank_core::agent_config::GithubSource> {
    clank::agent_store::load_agent_config(repo, &label("claude"))
        .unwrap()
        .map(|cfg| {
            cfg.wait_events
                .into_iter()
                .filter_map(|s| match s {
                    clank_core::agent_config::WaitEventSource::Github(g) => Some(g),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn starting_a_review_watches_that_pr_and_finishing_stops() {
    let (env, _) = env_with_pr(123);
    let repo = env.repo();
    assert!(watches(repo).is_empty());

    start_with(repo, "o/r", 123, None, None).unwrap();
    let armed = watches(repo);
    assert_eq!(
        armed.len(),
        1,
        "start writes the watch into master's config"
    );
    assert_eq!(armed[0].prs, [123]);
    let prompt = armed[0]
        .prompt
        .as_deref()
        .expect("a watch nobody wrote says what it is for");
    assert!(
        prompt.contains("Do not reply"),
        "awareness, not a conversation: {prompt}"
    );
    assert!(
        prompt.contains("first PARK"),
        "and it says what it did NOT see: a feed watch's first fetch is a silent \
         baseline, so activity while master was drafting is already history: {prompt}"
    );

    write_summary(repo, 123, "Looks good.");
    converge(repo, 123);
    let gh = FakeGh {
        pending: Some(42),
        ..Default::default()
    };
    clank::cli::pr_review::finish_through(repo, Some(env.home()), &label("claude"), Some(123), &gh)
        .unwrap();
    assert!(
        watches(repo).is_empty(),
        "handing the review over takes the watch with it — nothing to clean up by hand"
    );
}

#[test]
fn a_review_is_cleaned_up_from_the_config_that_received_it() {
    // Master can change while a review is open. Resolving master again
    // at the end would leave the old one's entry configured and delete
    // an unrelated watch from the new one's (codex ded5760).
    let (env, _) = env_with_pr(123);
    let repo = env.repo();
    start_with(repo, "o/r", 123, None, None).unwrap();
    assert_eq!(watches(repo).len(), 1, "armed in claude's config");

    // codex takes over as master, already watching this PR its own way.
    let mut theirs = clank::cli::pr_review::review_source("o/r", 123);
    theirs.prompt = Some("codex's own".into());
    clank::agent_store::update_agent_config(repo, &label("codex"), |cfg| {
        cfg.wait_events
            .push(clank_core::agent_config::WaitEventSource::Github(
                theirs.clone(),
            ));
    })
    .unwrap();
    clank::cli::agent::set_repo_master(repo, &label("codex")).unwrap();

    abort_with(repo, Some(env.home()), &label("codex"), Some(123)).unwrap();

    assert!(
        watches(repo).is_empty(),
        "the entry goes from the config that received it, not whoever is master now"
    );
    let theirs_now = clank::agent_store::load_agent_config(repo, &label("codex"))
        .unwrap()
        .map(|c| c.wait_events)
        .unwrap_or_default();
    assert_eq!(
        theirs_now,
        [clank_core::agent_config::WaitEventSource::Github(theirs)],
        "and the new master's own watch is not collateral"
    );
}

#[test]
fn aborting_a_review_stops_watching_too() {
    let (env, _) = env_with_pr(123);
    let repo = env.repo();
    start_with(repo, "o/r", 123, None, None).unwrap();
    assert_eq!(watches(repo).len(), 1);

    abort_with(repo, Some(env.home()), &label("claude"), Some(123)).unwrap();
    assert!(watches(repo).is_empty());
}

#[test]
fn a_watch_you_wrote_yourself_is_left_exactly_as_it_was() {
    let (env, _) = env_with_pr(123);
    let repo = env.repo();

    // The same WATCH, typed by hand — same repo, kinds and PR scope,
    // but your own wording and cadence. `source_key` covers none of
    // those, so downstream this IS the review's watch: a second entry
    // beside it would share one WAL, one horizon and one lease, and
    // the later prompt would win for both.
    let mut mine = clank::cli::pr_review::review_source("o/r", 123);
    mine.prompt = Some("my own words".into());
    mine.poll_interval = Some("5m".into());
    clank::agent_store::update_agent_config(repo, &label("claude"), |cfg| {
        cfg.wait_events
            .push(clank_core::agent_config::WaitEventSource::Github(
                mine.clone(),
            ));
    })
    .unwrap();

    start_with(repo, "o/r", 123, None, None).unwrap();
    assert_eq!(
        watches(repo),
        std::slice::from_ref(&mine),
        "nothing is added, and your prompt and interval are untouched"
    );

    abort_with(repo, Some(env.home()), &label("claude"), Some(123)).unwrap();
    assert_eq!(
        watches(repo),
        [mine],
        "and the review does not take away a watch it never gave you"
    );
}

#[test]
fn the_draft_watch_is_masters_alone() {
    let (env, _) = env_with_pr(123);
    let repo = env.repo();
    start_with(repo, "o/r", 123, None, None).unwrap();

    assert!(draft_watch_belongs_to(repo, &label("claude")));
    for not_master in ["codex", "ruthless"] {
        assert!(
            !draft_watch_belongs_to(repo, &label(not_master)),
            "{not_master} is a reviewer; the draft watch would write over master's horizon"
        );
    }
    assert!(
        !draft_watch_belongs_to(repo, &label("nobody")),
        "an agent the roster does not know cannot prove it owns the review"
    );
}

#[test]
fn starting_a_review_anchors_the_draft_baseline() {
    // Round 0 always hands master a PrMaster item, so the first wait
    // that PARKS comes after master has drafted. If the baseline were
    // taken then, an operator edit made in between would be swallowed
    // (codex 29431d9), so it is anchored at start instead.
    let (env, _) = env_with_pr(123);
    let repo = env.repo();
    start_with(repo, "o/r", 123, None, None).unwrap();

    let horizon = std::fs::read_to_string(draft_horizon(repo, 123)).expect("anchored");
    assert!(
        horizon.contains("absent"),
        "a review begins from NO draft, not from never having looked: {horizon}"
    );
}

#[test]
fn an_unusable_horizon_cannot_half_advance_a_round() {
    // It used to be able to: propose committed the round, then wrote
    // the horizon, and a failed write returned Err with the round
    // already advanced and verdicts already stale (codex ce892ea).
    // propose writes nothing now, so an unwritable draft.json is not
    // propose's problem at all.
    let (env, _) = env_with_pr(123);
    let repo = env.repo();
    start_with(repo, "o/r", 123, None, None).unwrap();
    let horizon = draft_horizon(repo, 123);
    std::fs::remove_file(&horizon).unwrap();
    std::fs::create_dir(&horizon).unwrap();

    assert_eq!(
        propose_with(repo, Some(env.home()), &label("claude"), Some(123)).unwrap(),
        1
    );
    assert_eq!(
        propose_with(repo, Some(env.home()), &label("claude"), Some(123)).unwrap(),
        2,
        "each propose advances the round exactly once"
    );
}

#[test]
fn abort_is_master_only() {
    let (env, _) = env_with_pr(123);
    let repo = env.repo();
    start_with(repo, "o/r", 123, None, None).unwrap();

    // A reviewer cannot abort.
    let err = abort_with(repo, Some(env.home()), &label("codex"), Some(123))
        .unwrap_err()
        .to_string();
    assert!(err.contains("master-only"), "got: {err}");
    assert!(
        repo.join(".clank/pr-reviews/123").exists(),
        "not deleted by reviewer"
    );

    // Master can.
    abort_with(repo, Some(env.home()), &label("claude"), Some(123)).unwrap();
    assert!(
        !repo.join(".clank/pr-reviews/123").exists(),
        "master abort removed it"
    );
}

#[test]
fn propose_is_master_only_and_bumps_round() {
    let (env, _) = env_with_pr(123);
    let repo = env.repo();
    start_with(repo, "o/r", 123, None, None).unwrap();

    // A reviewer cannot open the review.
    let err = propose_with(repo, Some(env.home()), &label("codex"), Some(123))
        .unwrap_err()
        .to_string();
    assert!(err.contains("master-only"), "got: {err}");

    // Master opens round 1, then re-opens round 2 after integrating.
    assert_eq!(
        propose_with(repo, Some(env.home()), &label("claude"), Some(123)).unwrap(),
        1
    );
    assert_eq!(
        propose_with(repo, Some(env.home()), &label("claude"), Some(123)).unwrap(),
        2
    );
}

#[test]
fn checkout_switches_branch_and_scaffolds_in_place() {
    use clank::cli::pr_review::checkout_with;
    let (env, pr_sha) = env_with_pr(123);
    let repo = env.repo();

    let dir = checkout_with(repo, 123).unwrap();
    assert!(dir.ends_with(".clank/pr-reviews/123"));
    // HEAD moved to the PR head on a fresh `pr-123` branch.
    assert_eq!(git_out(repo, &["rev-parse", "HEAD"]), pr_sha);
    assert_eq!(
        git_out(repo, &["rev-parse", "--abbrev-ref", "HEAD"]),
        "pr-123"
    );
    // Review scaffolded in place, pinned to the SAME head that was
    // checked out — one fetch (ruthless b88ae34).
    let state: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("pr.json")).unwrap()).unwrap();
    assert_eq!(
        state["head_sha"], pr_sha,
        "review pins the checked-out head"
    );
}

#[test]
fn checkout_refuses_already_started_review_without_switching_branch() {
    use clank::cli::pr_review::checkout_with;
    let (env, _) = env_with_pr(123);
    let repo = env.repo();
    // A review already exists for this PR. Commit the gitignore entry
    // start_with adds so the tree is clean (as in a real repo) — else
    // the dirty guard would fire before the already-started one.
    start_with(repo, "LLFourn/clank", 123, None, None).unwrap();
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "start review"]);
    let head_before = git_out(repo, &["rev-parse", "HEAD"]);

    let err = checkout_with(repo, 123).unwrap_err().to_string();
    assert!(err.contains("already started"), "got: {err}");
    // Refused BEFORE the checkout: HEAD unchanged (codex b944c58).
    assert_eq!(git_out(repo, &["rev-parse", "HEAD"]), head_before);
}

#[test]
fn checkout_refuses_dirty_worktree_before_fetching() {
    use clank::cli::pr_review::checkout_with;
    let (env, _) = env_with_pr(123);
    let repo = env.repo();
    let head_before = git_out(repo, &["rev-parse", "HEAD"]);

    std::fs::write(repo.join("file.rs"), "// uncommitted edit\n").unwrap();
    let err = checkout_with(repo, 123).unwrap_err().to_string();
    assert!(err.contains("dirty"), "got: {err}");
    // Refused BEFORE the checkout: HEAD unchanged, no review scaffold.
    assert_eq!(git_out(repo, &["rev-parse", "HEAD"]), head_before);
    assert!(!repo.join(".clank/pr-reviews/123").exists());
}

#[test]
fn note_resolution_errors_when_no_active_review() {
    let env = TestEnv::init();
    env.register_team("claude", &["codex"], &[]);
    let err = note_with(env.repo(), &label("codex"), None, Verdict::Finished, "x")
        .unwrap_err()
        .to_string();
    assert!(err.contains("no active PR review"), "got: {err}");
}

#[test]
fn pr_review_inputs_project_current_round_verdicts() {
    let (env, _) = env_with_pr(123);
    let repo = env.repo();
    start_with(repo, "o/r", 123, None, None).unwrap();

    // Fresh review: one input, round 0, no current verdicts.
    let inputs = clank::cli::pr_review::pr_review_inputs(repo);
    assert_eq!(inputs.len(), 1);
    assert_eq!((inputs[0].pr, inputs[0].round), (123, 0));
    assert!(inputs[0].current_verdicts.is_empty());

    // codex notes FINISHED at round 0 → it shows as a current verdict.
    note_with(repo, &label("codex"), Some(123), Verdict::Finished, "ok").unwrap();
    let inputs = clank::cli::pr_review::pr_review_inputs(repo);
    assert_eq!(inputs[0].current_verdicts.len(), 1);
    assert_eq!(inputs[0].current_verdicts[0].author, label("codex"));
    assert_eq!(inputs[0].current_verdicts[0].verdict, Verdict::Finished);
}

#[test]
fn pr_review_inputs_drop_stale_round_verdicts() {
    let (env, _) = env_with_pr(123);
    let repo = env.repo();
    start_with(repo, "o/r", 123, None, None).unwrap();
    note_with(repo, &label("codex"), Some(123), Verdict::Finished, "ok").unwrap();

    // Master revises → `propose` bumps the round to 1.
    propose_with(repo, Some(env.home()), &label("claude"), Some(123)).unwrap();

    // codex's round-0 verdict is now STALE → not a current verdict.
    let inputs = clank::cli::pr_review::pr_review_inputs(repo);
    assert_eq!(inputs[0].round, 1);
    assert!(
        inputs[0].current_verdicts.is_empty(),
        "stale round-0 verdict must not count for round 1"
    );
}

/// A scripted handoff boundary: records what the remote half was
/// asked to do, and can fail on demand. There is no submit here to
/// script — the trait has no such method, which is the plan's claim
/// made structural.
#[derive(Default)]
struct FakeGh {
    pending: Option<u64>,
    replies: usize,
    thumbs: usize,
    /// What `set_summary` fails with, VERBATIM. A bool would only
    /// say that it failed, and the whole point here is which sentence
    /// GitHub answers with: the missing-body refusal is the one
    /// `finish` must treat differently from every other 422.
    summary_error: Option<String>,
    calls: std::sync::Mutex<Vec<String>>,
}

impl clank::cli::pr_review::Handoff for FakeGh {
    fn pending(
        &self,
        _slug: &str,
        _pr: u32,
    ) -> anyhow::Result<Option<clank::cli::pr_review::gh::PendingReview>> {
        self.calls.lock().unwrap().push("pending".into());
        Ok(self
            .pending
            .map(|id| clank::cli::pr_review::gh::PendingReview {
                id,
                node_id: "PRR_x".into(),
            }))
    }
    fn open_with_summary(
        &self,
        _slug: &str,
        _pr: u32,
        head_sha: &str,
        body: &str,
    ) -> anyhow::Result<()> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("open:{head_sha}:{body}"));
        Ok(())
    }
    fn set_summary(&self, _slug: &str, _pr: u32, id: u64, body: &str) -> anyhow::Result<()> {
        self.calls.lock().unwrap().push(format!("set:{id}:{body}"));
        if let Some(said) = &self.summary_error {
            // A `GhFailure`, not a bare error: production classifies
            // by downcasting to it and reading `said` ALONE. The argv
            // carries the summary, so building the real one here is
            // what puts this fake on the same boundary the classifier
            // meets in production (codex 904bc9b).
            return Err(clank::cli::pr_review::gh::GhFailure {
                said: said.clone(),
                argv: clank::cli::pr_review::gh::update_body_argv("o/r", 123, id, body),
            }
            .into());
        }
        Ok(())
    }
    fn replies_left(&self, _slug: &str, _pr: u32, _id: u64) -> anyhow::Result<usize> {
        self.calls.lock().unwrap().push("replies".into());
        Ok(self.replies)
    }
    fn sweep_thumbs(&self, _slug: &str, _pr: u32, _id: u64) -> anyhow::Result<usize> {
        self.calls.lock().unwrap().push("sweep".into());
        Ok(self.thumbs)
    }
}

/// Converge the gate so `finish` can run.
fn converge(repo: &Path, pr: u32) {
    propose_with(repo, None, &label("claude"), Some(pr)).unwrap();
    note_with(repo, &label("codex"), Some(pr), Verdict::Finished, "ok").unwrap();
    note_with(repo, &label("ruthless"), Some(pr), Verdict::Finished, "ok").unwrap();
}

fn write_summary(repo: &Path, pr: u32, text: &str) {
    let md = repo.join(format!(".clank/pr-reviews/{pr}/master.md"));
    let body = std::fs::read_to_string(&md).unwrap().replace(
        "<the overview that goes on the draft review itself — the human reads
it above your comments and submits, or does not>",
        text,
    );
    std::fs::write(&md, body).unwrap();
}

#[test]
fn a_converged_review_is_handed_over_as_a_draft() {
    let (env, _) = env_with_pr(123);
    let repo = env.repo();
    start_with(repo, "o/r", 123, None, None).unwrap();
    write_summary(repo, 123, "The refactor is clean.");
    converge(repo, 123);

    let gh = FakeGh {
        pending: Some(42),
        replies: 2,
        thumbs: 3,
        ..Default::default()
    };
    clank::cli::pr_review::finish_through(repo, Some(env.home()), &label("claude"), Some(123), &gh)
        .unwrap();

    let calls = gh.calls.lock().unwrap().clone();
    assert!(
        calls.contains(&"set:42:The refactor is clean.".to_string()),
        "the summary goes onto the existing draft: {calls:?}",
    );
    // The scaffolding comes off BEFORE the summary is written: the
    // draft is clean by the time it carries the overview.
    let sweep = calls.iter().position(|c| c == "sweep");
    let set = calls.iter().position(|c| c.starts_with("set:"));
    assert!(
        sweep.is_some() && sweep < set,
        "the thumbs are swept, and swept first: {calls:?}",
    );
    assert!(
        !repo.join(".clank/pr-reviews/123").exists(),
        "the scratch is retired once the remote work is done",
    );
}

#[test]
fn a_failed_handoff_keeps_the_scratch_and_the_retry_works() {
    let (env, _) = env_with_pr(123);
    let repo = env.repo();
    start_with(repo, "o/r", 123, None, None).unwrap();
    write_summary(repo, 123, "Needs another look at the parser.");
    converge(repo, 123);

    let broken = FakeGh {
        pending: Some(42),
        summary_error: Some("gh exploded".into()),
        ..Default::default()
    };
    let err = clank::cli::pr_review::finish_through(
        repo,
        Some(env.home()),
        &label("claude"),
        Some(123),
        &broken,
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("gh exploded"), "{err}");
    assert!(
        repo.join(".clank/pr-reviews/123/master.md").is_file(),
        "a failed handoff strands nothing: the scratch is still there to retry",
    );

    let working = FakeGh {
        pending: Some(42),
        ..Default::default()
    };
    clank::cli::pr_review::finish_through(
        repo,
        Some(env.home()),
        &label("claude"),
        Some(123),
        &working,
    )
    .unwrap();
    assert!(!repo.join(".clank/pr-reviews/123").exists());
    let calls = working.calls.lock().unwrap().clone();
    assert!(
        calls
            .iter()
            .any(|c| c.contains("Needs another look at the parser")),
        "the retry carried the same summary: {calls:?}",
    );
}

#[test]
fn a_review_with_no_comments_is_handed_over_as_a_summary() {
    let (env, pr_sha) = env_with_pr(123);
    let repo = env.repo();
    start_with(repo, "o/r", 123, None, None).unwrap();
    write_summary(repo, 123, "Read it end to end; nothing to raise.");
    converge(repo, 123);

    // No pending review: nobody drafted an inline comment.
    let gh = FakeGh::default();
    clank::cli::pr_review::finish_through(repo, Some(env.home()), &label("claude"), Some(123), &gh)
        .unwrap();

    // The PINNED sha reaches the boundary: the draft is attached to
    // what the team read, not to wherever the PR has moved since —
    // and the pin dies with the scratch, so nothing can recover it
    // afterwards.
    let calls = gh.calls.lock().unwrap().clone();
    assert!(
        calls.contains(&format!(
            "open:{pr_sha}:Read it end to end; nothing to raise."
        )),
        "a summary-only draft is opened, pinned, rather than the summary being lost: {calls:?}",
    );
    assert!(!repo.join(".clank/pr-reviews/123").exists());
}

#[test]
fn a_second_finish_touches_nothing_remote() {
    let (env, _) = env_with_pr(123);
    let repo = env.repo();
    start_with(repo, "o/r", 123, None, None).unwrap();
    write_summary(repo, 123, "Done.");
    converge(repo, 123);
    let first = FakeGh {
        pending: Some(42),
        ..Default::default()
    };
    clank::cli::pr_review::finish_through(
        repo,
        Some(env.home()),
        &label("claude"),
        Some(123),
        &first,
    )
    .unwrap();

    let second = FakeGh {
        pending: Some(42),
        ..Default::default()
    };
    let err = clank::cli::pr_review::finish_through(
        repo,
        Some(env.home()),
        &label("claude"),
        Some(123),
        &second,
    )
    .unwrap_err();
    assert!(
        second.calls.lock().unwrap().is_empty(),
        "it refuses before touching the remote: {err}",
    );
}

#[test]
fn finish_is_master_only_and_refuses_until_converged() {
    use clank::cli::pr_review::finish_with;
    let (env, _) = env_with_pr(123);
    let repo = env.repo();
    start_with(repo, "o/r", 123, None, None).unwrap();

    // A reviewer cannot hand the review over. (These refuse before
    // any gh call.)
    let err = finish_with(repo, Some(env.home()), &label("codex"), Some(123))
        .unwrap_err()
        .to_string();
    assert!(err.contains("master-only"), "got: {err}");

    // Master, but not converged (no verdicts) → refuses BEFORE any
    // gh call (fails the gate check).
    let err = finish_with(repo, Some(env.home()), &label("claude"), Some(123))
        .unwrap_err()
        .to_string();
    assert!(err.contains("not converged"), "got: {err}");

    // Even one tier finished isn't enough (ruthless still owes).
    note_with(repo, &label("codex"), Some(123), Verdict::Finished, "ok").unwrap();
    let err = finish_with(repo, Some(env.home()), &label("claude"), Some(123))
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("not converged"),
        "commit-only finished still blocks: {err}"
    );
    // The scratch is untouched by a refused handoff.
    assert!(repo.join(".clank/pr-reviews/123").exists());
}

#[test]
fn status_fails_closed_without_a_team() {
    // codex da7ab89: a missing/misconfigured team must NOT make
    // status print "converged" off an empty reviewer set. Start a
    // review but register no team → status errors, never "done".
    let env = TestEnv::init();
    setup_repo_with_pr(&env, 123);
    start_with(env.repo(), "o/r", 123, None, None).unwrap();
    let res = status_with(env.repo(), Some(env.home()), Some(123));
    let err = res.expect_err("status must fail closed without a team");
    assert!(
        !err.to_string().contains("converged"),
        "must not fabricate convergence: {err}"
    );
}

/// GitHub's refusal, verbatim, as the probe on a live PR recorded it
/// (LLFourn/clank#3, since deleted). REST `PUT` and GraphQL
/// `updatePullRequestReview` both answer with this sentence when the
/// review was created without a body.
const MISSING_BODY: &str = "HTTP 422: Could not edit a review with a missing body \
    (https://api.github.com/repos/o/r/pulls/123/reviews/42)";

/// A review the loop created bodyless cannot be handed over, and
/// master must stop being woken to try.
///
/// The gate stays Finished and the scratch only goes on a handoff
/// that WORKED, so without suppression the converged review emits
/// `Finish` on every arm forever — which is what the bug report
/// described: "finish exits 1 without recording anything, so the wake
/// item re-fires on every Stop, forever."
#[test]
fn a_handoff_that_cannot_work_stops_waking_master() {
    let (env, _) = env_with_pr(123);
    let repo = env.repo();
    start_with(repo, "o/r", 123, None, None).unwrap();
    write_summary(repo, 123, "Needs another look at the parser.");
    converge(repo, 123);

    // Converged and not yet attempted: master is asked to finish.
    let finish_items = |repo: &Path| -> usize {
        clank::cli::pr_review::pr_review_inputs(repo)
            .into_iter()
            .filter(|i| i.handoff_blocked)
            .count()
    };
    assert_eq!(
        finish_items(repo),
        0,
        "nothing is blocked before the attempt"
    );

    let bodyless = FakeGh {
        pending: Some(42),
        summary_error: Some(MISSING_BODY.into()),
        ..Default::default()
    };
    let err = clank::cli::pr_review::finish_through(
        repo,
        Some(env.home()),
        &label("claude"),
        Some(123),
        &bodyless,
    )
    .unwrap_err()
    .to_string();
    assert!(
        err.contains("created without a body"),
        "the cause is named, not left as a 422: {err}"
    );
    assert!(
        !err.to_lowercase().contains("github's interface")
            && !err.to_lowercase().contains("submit"),
        "no repair is prescribed -- entering a summary in the UI submits, and a published \
         review cannot be recalled: {err}"
    );

    // Everything local survives: the draft is still on GitHub, the
    // summary is still here, and the review is still a review.
    assert!(
        repo.join(".clank/pr-reviews/123/master.md").is_file(),
        "the summary must survive a blocked handoff",
    );

    // THE POINT: asked again and again, the projection no longer
    // offers the handoff.
    for _ in 0..3 {
        assert_eq!(
            finish_items(repo),
            1,
            "a handoff that cannot work must not keep waking master",
        );
    }

    // And the operator can SEE it. Once the failure scrolls out of
    // the terminal, status is the only place left that knows master
    // has deliberately stopped (codex 904bc9b).
    let said = status_with(repo, Some(env.home()), Some(123)).unwrap();
    assert!(
        said.contains("handoff BLOCKED"),
        "pr-review status must say the handoff stopped: {said}"
    );
    assert!(
        said.contains("converged"),
        "and convergence stays a separate fact -- the team did agree: {said}"
    );
    assert!(
        said.contains("clank pr-review finish"),
        "with the one thing that runs it again: {said}"
    );
}

/// Only the DIAGNOSED refusal blocks. An ordinary failure is still
/// worth retrying, and must keep its wake.
#[test]
fn an_ordinary_handoff_failure_still_wakes_master() {
    let (env, _) = env_with_pr(123);
    let repo = env.repo();
    start_with(repo, "o/r", 123, None, None).unwrap();
    write_summary(repo, 123, "Needs another look at the parser.");
    converge(repo, 123);

    // Including another 422: the STATUS is not the diagnosis, the
    // sentence is. Blocking on the code would silence refusals that
    // are worth retrying.
    for transient in [
        "HTTP 502: Bad Gateway",
        "HTTP 422: Validation Failed (some other reason entirely)",
        "error: connection reset",
    ] {
        let flaky = FakeGh {
            pending: Some(42),
            summary_error: Some(transient.into()),
            ..Default::default()
        };
        assert!(
            clank::cli::pr_review::finish_through(
                repo,
                Some(env.home()),
                &label("claude"),
                Some(123),
                &flaky,
            )
            .is_err()
        );
        assert!(
            clank::cli::pr_review::pr_review_inputs(repo)
                .into_iter()
                .all(|i| !i.handoff_blocked),
            "`{transient}` keeps its retry: suppressing it would strand the review",
        );
    }
}

/// A summary that QUOTES GitHub's refusal must not make a transient
/// failure permanent.
///
/// The argv carries `body=<the summary>`, and a summary is free text
/// an agent wrote — a review of this very bug would quote the refusal
/// in it. Classifying on the formatted error chain reads our own
/// request back as the server's verdict (codex 904bc9b).
#[test]
fn a_summary_quoting_the_refusal_keeps_its_retry() {
    let (env, _) = env_with_pr(123);
    let repo = env.repo();
    start_with(repo, "o/r", 123, None, None).unwrap();
    write_summary(
        repo,
        123,
        "The handoff broke: GitHub answers \"Could not edit a review with a missing body\".",
    );
    converge(repo, 123);

    let flaky = FakeGh {
        pending: Some(42),
        summary_error: Some("HTTP 502: Bad Gateway".into()),
        ..Default::default()
    };
    let err = clank::cli::pr_review::finish_through(
        repo,
        Some(env.home()),
        &label("claude"),
        Some(123),
        &flaky,
    )
    .unwrap_err();
    assert!(
        format!("{err:#}").contains("Could not edit a review with a missing body"),
        "the fixture must really carry the phrase in the request: {err:#}"
    );
    assert!(
        clank::cli::pr_review::pr_review_inputs(repo)
            .into_iter()
            .all(|i| !i.handoff_blocked),
        "a 502 stays retryable however the summary is worded",
    );
    assert!(
        repo.join(".clank/pr-reviews/123/master.md").is_file(),
        "and the review survives it",
    );
}

/// The block belongs to the review that earned it.
///
/// `start` opens a NEW review with a new instance, and a block
/// recorded against the old one must not silence it — the stored
/// instance is what makes that automatic rather than remembered.
#[test]
fn a_later_review_does_not_inherit_the_block() {
    let (env, _) = env_with_pr(123);
    let repo = env.repo();
    start_with(repo, "o/r", 123, None, None).unwrap();
    write_summary(repo, 123, "Needs another look at the parser.");
    converge(repo, 123);

    let bodyless = FakeGh {
        pending: Some(42),
        summary_error: Some(MISSING_BODY.into()),
        ..Default::default()
    };
    assert!(
        clank::cli::pr_review::finish_through(
            repo,
            Some(env.home()),
            &label("claude"),
            Some(123),
            &bodyless,
        )
        .is_err()
    );
    assert!(
        clank::cli::pr_review::pr_review_inputs(repo)
            .into_iter()
            .any(|i| i.handoff_blocked)
    );

    // A fresh review on the same PR: a different instance, so the old
    // block does not reach it.
    abort_with(repo, Some(env.home()), &label("claude"), Some(123)).unwrap();
    start_with(repo, "o/r", 123, None, None).unwrap();
    assert!(
        clank::cli::pr_review::pr_review_inputs(repo)
            .into_iter()
            .all(|i| !i.handoff_blocked),
        "a new review inherits nothing from the one it replaced",
    );
}

/// Running `finish` again is the operator asking, so the block lifts
/// for that attempt — and a run that then fails transiently leaves
/// master wakeable rather than silently blocked.
#[test]
fn an_explicit_retry_lifts_the_block() {
    let (env, _) = env_with_pr(123);
    let repo = env.repo();
    start_with(repo, "o/r", 123, None, None).unwrap();
    write_summary(repo, 123, "Needs another look at the parser.");
    converge(repo, 123);

    let bodyless = FakeGh {
        pending: Some(42),
        summary_error: Some(MISSING_BODY.into()),
        ..Default::default()
    };
    let run = |gh: &FakeGh| {
        clank::cli::pr_review::finish_through(
            repo,
            Some(env.home()),
            &label("claude"),
            Some(123),
            gh,
        )
    };
    assert!(run(&bodyless).is_err());
    assert!(
        clank::cli::pr_review::pr_review_inputs(repo)
            .into_iter()
            .any(|i| i.handoff_blocked)
    );

    // A retry that hits something else clears the block: the reason
    // for suppression is gone, so the wake comes back.
    let flaky = FakeGh {
        pending: Some(42),
        summary_error: Some("HTTP 502: Bad Gateway".into()),
        ..Default::default()
    };
    assert!(run(&flaky).is_err());
    assert!(
        clank::cli::pr_review::pr_review_inputs(repo)
            .into_iter()
            .all(|i| !i.handoff_blocked),
        "the diagnosis no longer holds, so master is wakeable again",
    );

    // And a retry that works hands over as normal.
    let working = FakeGh {
        pending: Some(42),
        ..Default::default()
    };
    run(&working).unwrap();
    assert!(
        !repo.join(".clank/pr-reviews/123").exists(),
        "a handoff that worked takes the scratch with it",
    );
}
