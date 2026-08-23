//! `clank wait` re-derives role/tiers/policy on every wake
//! (wait-reloads-config-per-refold): a parked wait must project
//! against the CURRENT team, not the one captured at arm time. All
//! in-process (no binary spawning), poll mode for determinism.

mod common;
use common::TestEnv;

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use clank::cli::WaitArgs;
use clank::cli::teams_config::{AgentDescription, RosterRole};
use clank_core::vocab::Tool;

fn git(repo: &Path, args: &[&str]) {
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .status()
            .unwrap()
            .success(),
        "git {args:?} failed"
    );
}

fn write(repo: &Path, rel: &str, body: &str) {
    let abs = repo.join(rel);
    std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
    std::fs::write(abs, body).unwrap();
}

fn wait_args(repo: &Path, author: &str) -> WaitArgs {
    WaitArgs {
        repo: Some(repo.to_path_buf()),
        author: Some(author.into()),
        die_with_owner: false,
        json: true,
        events: Vec::new(),
        r#for: None,
        peek: false,
        no_cache: false,
        poll: true,
        no_poll: false,
    }
}

/// A repo where `rev` (commit reviewer) has NO work — no plans — but a
/// queued draft means a MASTER would immediately get PromoteFromQueue.
/// The promotion-flip fixtures build on this.
fn parked_reviewer_env() -> TestEnv {
    let env = TestEnv::init();
    env.register_team("master", &["rev"], &[]);
    let repo = env.repo();
    write(repo, ".clank/.gitignore", "/cache/\n/agents/\n/shelved/\n");
    write(repo, "seed.txt", "seed");
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "scaffold"]);
    // Queue is gitignored runtime state; a queued name is what makes
    // the MASTER projection non-empty (PromoteFromQueue).
    write(repo, ".clank/queue/500-someplan.md", "# someplan\n");
    env
}

fn promote_rev(repo: &Path) {
    clank::cli::agent::set_repo_master(repo, &clank_core::ids::AgentLabel::parse("rev").unwrap())
        .expect("promote rev to master");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn omitted_role_re_resolves_when_the_author_is_promoted_mid_wait() {
    // codex 1988b3e case 1: role OMITTED. As a commit reviewer with no
    // plans, rev parks; promoting rev mid-wait must re-resolve the
    // role to MASTER on the wake and return the queue-promote item —
    // without the wait restarting.
    let env = parked_reviewer_env();
    let repo = env.repo();
    common::assert_stays_parked(repo, wait_args(repo, "rev"), "reviewer with no plans").await;

    let waiter = tokio::spawn(clank::cli::wait::run(wait_args(repo, "rev")));
    tokio::time::sleep(Duration::from_millis(1200)).await;
    promote_rev(repo);
    tokio::time::timeout(common::race_deadline(repo), waiter)
        .await
        .expect("promotion must wake the parked wait as MASTER")
        .expect("join")
        .expect("master projection returns the queue item");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parked_wait_survives_a_transient_config_corruption() {
    // codex 1988b3e case 3: the fail-soft branch. Corrupting the repo
    // config mid-wait makes derivation Err — the wait must keep its
    // last-known-good inputs and stay parked (not die); repairing the
    // config (with a promotion) must then wake it normally.
    let env = parked_reviewer_env();
    let repo = env.repo();
    let cfg_path = repo.join(".clank/config.json");
    let good = std::fs::read(&cfg_path).expect("fixture config exists");

    let waiter = tokio::spawn(clank::cli::wait::run(wait_args(repo, "rev")));
    tokio::time::sleep(Duration::from_millis(1200)).await;
    std::fs::write(&cfg_path, b"{ this is not json").unwrap();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(
        !waiter.is_finished(),
        "corrupted config must not kill or wake the parked wait"
    );
    std::fs::write(&cfg_path, &good).unwrap();
    promote_rev(repo);
    tokio::time::timeout(common::race_deadline(repo), waiter)
        .await
        .expect("repaired config must wake the wait")
        .expect("join")
        .expect("wait returns under the repaired, re-derived config");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parked_wait_picks_up_a_tier_change_without_restarting() {
    let _serial = common::serial();
    let env = TestEnv::init();
    // `rev` starts in the final tier, which has no work on an intro
    // commit. Moving it to commit must wake the existing wait and
    // project that intro without relying on an off-roster identity.
    env.register_team("master", &["other"], &[]);
    let repo = env.repo();
    clank::cli::agent::add_repo_roster_agent(
        repo,
        &clank_core::ids::AgentLabel::parse("rev").unwrap(),
        AgentDescription {
            tool: Tool::Claude,
            launch: None,
            initial_prompt: None,
        },
        RosterRole::Final,
    )
    .unwrap();
    write(repo, ".clank/.gitignore", "/cache/\n/agents/\n/shelved/\n");
    write(repo, ".clank/plans/foo.md", "# foo\n");
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "[foo] intro"]);

    // Sanity: under the CURRENT tiers the wait truly parks.
    common::assert_stays_parked(
        repo,
        wait_args(repo, "rev"),
        "final reviewer before the final gate",
    )
    .await;

    // Park a wait, then MOVE rev to the commit tier mid-wait. The
    // config write wakes the loop; the re-derived tiers must project
    // the review item — the wait returns WITHOUT restarting. (Before
    // wait-reloads-config-per-refold this hung to timeout on the
    // arm-time tiers.)
    let repo_owned = repo.to_path_buf();
    let waiter = tokio::spawn(clank::cli::wait::run(wait_args(repo, "rev")));
    tokio::time::sleep(Duration::from_millis(1200)).await;
    clank::cli::agent::set_repo_review(
        &repo_owned,
        &clank_core::ids::AgentLabel::parse("rev").unwrap(),
        clank::cli::teams_config::ReviewKind::Commit,
    )
    .expect("flip rev to the commit tier");

    tokio::time::timeout(common::race_deadline(repo), waiter)
        .await
        .expect("the parked wait must wake and project the NEW tiers")
        .expect("join")
        .expect("wait returns the review item under the new config");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parked_wait_exits_when_its_master_identity_is_swapped_out() {
    let _serial = common::serial();
    let env = parked_reviewer_env();
    let repo = env.repo();
    // Remove the queue item so the master has no immediate work and parks.
    std::fs::remove_file(repo.join(".clank/queue/500-someplan.md")).unwrap();
    clank::cli::agent::declare_global_agent(
        env.home(),
        &clank_core::ids::AgentLabel::parse("in").unwrap(),
        AgentDescription {
            tool: Tool::Claude,
            launch: None,
            initial_prompt: None,
        },
    )
    .unwrap();

    let waiter = tokio::spawn(clank::cli::wait::run(wait_args(repo, "master")));
    tokio::time::sleep(common::parked_window(repo)).await;
    assert!(
        !waiter.is_finished(),
        "master wait must be parked before the swap"
    );

    clank::cli::agent::swap_repo_agent(
        repo,
        Some(env.home()),
        &clank_core::ids::AgentLabel::parse("master").unwrap(),
        &clank_core::ids::AgentLabel::parse("in").unwrap(),
    )
    .unwrap();

    let err = tokio::time::timeout(common::race_deadline(repo), waiter)
        .await
        .expect("roster change must wake the parked wait")
        .expect("join")
        .expect_err("a removed identity must terminate instead of becoming a reviewer");
    assert!(
        matches!(
            err.downcast_ref::<clank::agent_store::RoleResolutionError>(),
            Some(clank::agent_store::RoleResolutionError::NotRegistered { .. })
        ),
        "typed off-roster error: {err:#}"
    );
    assert!(format!("{err:#}").contains("not on this repo's roster"));
}
