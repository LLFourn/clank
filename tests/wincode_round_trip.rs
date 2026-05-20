//! Phase 2 of cache-core-fold-and-live-feedback: prove the
//! commit-derived `BaseRepoState` round-trips through wincode for
//! every model variant the fold can produce.
//!
//! **Important**: these tests exercise the BASE boundary, not
//! `rebuild_repo`. The cache only persists commit-derived state;
//! live feedback files must not enter the payload. Tests therefore
//! call `git_io::snapshot` + `derive_base_state` directly and
//! assert the resulting payload's gates carry empty `feedback`
//! maps even when feedback files exist on disk.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use trinity::disk_snapshot::derive_base_state;
use trinity::git_io;
use trinity::lifecycle::PlanKey;
use trinity::repo_state::BaseRepoState;
use trinity_core::ids::{AgentLabel, CommitSha, PlanKey as CorePlanKey};
use trinity_core::model::Plan;
use wincode::{SchemaRead, SchemaWrite};

/// Mirror of the production cache payload defined in
/// `src/state_cache.rs`. Reproduced here so the test asserts the
/// payload shape AND the wincode derives on the underlying core
/// types compose correctly — the production type is private to
/// `state_cache`, so this is the public proxy.
///
/// If the production payload shape changes, this fixture must
/// change in lockstep.
#[derive(Debug, Clone, PartialEq, Eq, SchemaWrite, SchemaRead)]
struct BaseStatePayloadFixture {
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

/// Build a `BaseRepoState` the same way the cache will: via the
/// commit-snapshot + feedback-blind fold. **Does not call
/// `rebuild_repo`** — that returns live state with feedback
/// attached, which is not what we serialize.
async fn build_base(repo: &Path) -> BaseRepoState {
    let snap = git_io::snapshot(repo).await.expect("snapshot");
    derive_base_state(repo.to_path_buf(), snap)
}

fn payload_from_base(base: &BaseRepoState) -> BaseStatePayloadFixture {
    BaseStatePayloadFixture {
        head: base.head.clone(),
        plans: base.plans.clone(),
    }
}

fn round_trip(payload: &BaseStatePayloadFixture) {
    let bytes = wincode::serialize(payload).expect("wincode encode");
    let back: BaseStatePayloadFixture = wincode::deserialize(&bytes).expect("wincode decode");
    assert_eq!(payload, &back, "wincode round-trip diverged");
    assert!(!bytes.is_empty(), "encoded payload must be non-empty");
}

#[tokio::test]
async fn round_trip_plan_only_variant() {
    let dir = init_repo();
    write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
    commit(dir.path(), "Add foo plan");

    let base = build_base(dir.path()).await;
    round_trip(&payload_from_base(&base));
}

#[tokio::test]
async fn round_trip_code_only_and_mixed_variants() {
    let dir = init_repo();
    write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
    commit(dir.path(), "Intro foo");
    write_file(dir.path(), "src/lib.rs", "fn main() {}\n");
    commit(dir.path(), "Implement foo");
    write_file(dir.path(), ".trinity/plans/foo.md", "# foo v2\n");
    write_file(dir.path(), "src/lib.rs", "fn main() { /* v2 */ }\n");
    commit(dir.path(), "Refine foo");

    let base = build_base(dir.path()).await;
    round_trip(&payload_from_base(&base));
}

#[tokio::test]
async fn round_trip_multi_plan_variant() {
    let dir = init_repo();
    write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
    write_file(dir.path(), ".trinity/plans/bar.md", "# bar\n");
    commit(dir.path(), "Intro foo and bar"); // MultiPlan

    let base = build_base(dir.path()).await;
    round_trip(&payload_from_base(&base));
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

    let base = build_base(dir.path()).await;
    let key = PlanKey::parse("foo").unwrap();
    assert!(base.plans[&key].is_frozen());
    assert!(!base.plans[&key].archived_cycles.is_empty());
    round_trip(&payload_from_base(&base));
}

/// The core cache invariant: a feedback file on disk must NOT
/// enter the base payload. Even when `.trinity/feedback/...` is
/// present, `derive_base_state`'s gates ship empty `feedback`
/// maps. This is the property the cache write/load lifecycle
/// relies on for correctness.
#[tokio::test]
async fn live_feedback_does_not_enter_base_payload() {
    let dir = init_repo();
    write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
    commit(dir.path(), "Intro foo");

    // Get the intro sha from a quick fold so the feedback path
    // targets a real commit on the timeline.
    let pre_base = build_base(dir.path()).await;
    let intro = pre_base.plans[&PlanKey::parse("foo").unwrap()]
        .plan_intro
        .clone();
    let fb_path = format!(".trinity/feedback/foo/{}/alice.md", intro.as_str());
    write_file(dir.path(), &fb_path, "APPROVE\n\nlgtm\n");

    // Re-fold the base. The on-disk feedback file is present, but
    // `derive_base_state` is feedback-blind by contract.
    let base = build_base(dir.path()).await;
    let plan = &base.plans[&PlanKey::parse("foo").unwrap()];
    let gate = plan
        .timeline
        .iter()
        .find_map(|e| e.gate())
        .expect("intro is reviewable");
    assert!(
        gate.feedback.is_empty(),
        "base payload must NOT carry live feedback; got {:?}",
        gate.feedback.keys().collect::<Vec<_>>(),
    );
    assert!(
        !gate
            .participants
            .contains(&AgentLabel::parse("alice").unwrap()),
        "base payload participants must be empty before live overlay",
    );

    // Round-trip the (correctly empty) base payload.
    round_trip(&payload_from_base(&base));
}
