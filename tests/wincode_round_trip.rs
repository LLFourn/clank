//! Phase 2 of cache-core-fold-and-live-feedback: pin that the
//! commit-derived `BaseRepoState` round-trips through wincode for
//! every model variant the fold can produce. The cache payload in
//! Phase 3 will be a rootless mirror of `RepoState`; this test
//! exercises the underlying type derives.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use trinity::lifecycle::PlanKey;
use trinity::rebuild::rebuild_repo;
use trinity_core::ids::{AgentLabel, CommitSha, PlanKey as CorePlanKey};
use trinity_core::model::Plan;
use wincode::{SchemaRead, SchemaWrite};

/// Same shape Phase 3's cache module will write — `RepoState` minus
/// the absolute `root`. Defined here as a fixture; the production
/// version lives in `state_cache.rs` once Phase 3 lands.
#[derive(Debug, Clone, PartialEq, Eq, SchemaWrite, SchemaRead)]
struct RootlessBasePayload {
    head: Option<CommitSha>,
    plans: BTreeMap<CorePlanKey, Plan>,
}

fn run_git(cwd: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .status()
        .expect("git");
    assert!(status.success(), "git {args:?} failed");
}

fn write_file(repo: &Path, rel: &str, body: &str) {
    let abs = repo.join(rel);
    if let Some(parent) = abs.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(abs, body).unwrap();
}

fn init_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path();
    run_git(p, &["init", "--quiet", "--initial-branch=main"]);
    run_git(p, &["config", "user.email", "test@test"]);
    run_git(p, &["config", "user.name", "test"]);
    run_git(p, &["config", "commit.gpgsign", "false"]);
    dir
}

fn commit(repo: &Path, msg: &str) {
    run_git(repo, &["add", "-A"]);
    run_git(repo, &["commit", "--quiet", "-m", msg]);
}

fn payload_from_state(state: &trinity::repo_state::RepoState) -> RootlessBasePayload {
    RootlessBasePayload {
        head: state.head.clone(),
        plans: state.plans.clone(),
    }
}

fn round_trip(payload: &RootlessBasePayload) {
    let bytes = wincode::serialize(payload).expect("wincode encode");
    let back: RootlessBasePayload = wincode::deserialize(&bytes).expect("wincode decode");
    assert_eq!(payload, &back, "wincode round-trip diverged");
    assert!(!bytes.is_empty(), "encoded payload must be non-empty");
}

#[tokio::test]
async fn round_trip_plan_only_variant() {
    let dir = init_repo();
    write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
    commit(dir.path(), "Add foo plan");

    let live = rebuild_repo(dir.path()).await.unwrap();
    round_trip(&payload_from_state(&live));
}

#[tokio::test]
async fn round_trip_code_only_and_mixed_variants() {
    let dir = init_repo();
    write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
    commit(dir.path(), "Intro foo");
    // CodeOnly
    write_file(dir.path(), "src/lib.rs", "fn main() {}\n");
    commit(dir.path(), "Implement foo");
    // Mixed: plan revision + code in same commit
    write_file(dir.path(), ".trinity/plans/foo.md", "# foo v2\n");
    write_file(dir.path(), "src/lib.rs", "fn main() { /* v2 */ }\n");
    commit(dir.path(), "Refine foo");

    let live = rebuild_repo(dir.path()).await.unwrap();
    round_trip(&payload_from_state(&live));
}

#[tokio::test]
async fn round_trip_multi_plan_variant() {
    let dir = init_repo();
    write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
    write_file(dir.path(), ".trinity/plans/bar.md", "# bar\n");
    commit(dir.path(), "Intro foo and bar"); // MultiPlan

    let live = rebuild_repo(dir.path()).await.unwrap();
    round_trip(&payload_from_state(&live));
}

#[tokio::test]
async fn round_trip_finalize_variant_with_archived_cycle() {
    let dir = init_repo();
    write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
    commit(dir.path(), "Intro foo");
    write_file(
        dir.path(),
        ".trinity/finished/foo/alice.md",
        "APPROVE\n\nlgtm\n",
    );
    commit(dir.path(), "Finalize foo");

    let live = rebuild_repo(dir.path()).await.unwrap();
    let key = PlanKey::parse("foo").unwrap();
    assert!(live.plans[&key].is_frozen());
    assert!(!live.plans[&key].archived_cycles.is_empty());
    round_trip(&payload_from_state(&live));
}

#[tokio::test]
async fn round_trip_preserves_gate_with_feedback() {
    let dir = init_repo();
    write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
    commit(dir.path(), "Intro foo");
    let live = rebuild_repo(dir.path()).await.unwrap();
    let intro = live.plans[&PlanKey::parse("foo").unwrap()]
        .plan_intro
        .clone();
    let path = format!(".trinity/feedback/foo/{}/alice.md", intro.as_str());
    write_file(dir.path(), &path, "APPROVE\n\nlgtm\n");

    // Note: the cache payload is the *base* state — feedback isn't
    // serialized. The base fold produces empty-feedback gates.
    // We still round-trip the gate structure to make sure the typed
    // enum tag + BTreeMap encoding survive even when feedback is
    // empty.
    let live = rebuild_repo(dir.path()).await.unwrap();
    let plan = &live.plans[&PlanKey::parse("foo").unwrap()];
    let gate = plan
        .timeline
        .iter()
        .find_map(|e| e.gate())
        .expect("intro gate");
    // Sanity: live state has the feedback attached.
    assert!(
        gate.feedback
            .contains_key(&AgentLabel::parse("alice").unwrap())
    );
    round_trip(&payload_from_state(&live));
}
