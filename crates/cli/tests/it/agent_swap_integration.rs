//! `clank agent swap <out> <in>` — replace a roster member, keep the role.

use crate::common;

use clank::cli::teams_config::{AgentDescription, RosterRole};
use clank_core::ids::AgentLabel;
use clank_core::vocab::Tool;
use common::TestEnv;
use std::path::Path;

fn lbl(s: &str) -> AgentLabel {
    AgentLabel::parse(s).unwrap()
}

fn desc() -> AgentDescription {
    AgentDescription {
        tool: Tool::Claude,
        launch: None,
        initial_prompt: None,
    }
}

/// Put `label` in the user-scope library so a swap can copy its
/// description, the way `agent add <name>` does.
fn declare(home: &Path, label: &str) {
    clank::cli::agent::declare_global_agent(home, &lbl(label), desc()).unwrap();
}

fn roster(repo: &Path) -> Vec<(String, RosterRole)> {
    clank::agent_store::load_repo_config_required(repo)
        .unwrap()
        .agents
        .iter()
        .map(|(l, a)| (l.as_str().to_string(), a.role))
        .collect()
}

fn swap(env: &TestEnv, out: &str, into: &str) -> anyhow::Result<()> {
    clank::cli::agent::swap_repo_agent(env.repo(), Some(env.home()), &lbl(out), &lbl(into))
}

#[test]
fn swap_carries_the_role_across_for_every_reviewer_tier() {
    for tier in [
        RosterRole::Commit,
        RosterRole::Plan,
        RosterRole::Final,
        RosterRole::Gate,
    ] {
        let env = TestEnv::init();
        clank::cli::agent::add_repo_roster_agent(env.repo(), &lbl("claude"), desc(), tier).unwrap();
        clank::cli::agent::set_repo_master(env.repo(), &lbl("claude")).unwrap();
        clank::cli::agent::add_repo_roster_agent(env.repo(), &lbl("out"), desc(), tier).unwrap();
        declare(env.home(), "in");

        swap(&env, "out", "in").unwrap();

        let after = roster(env.repo());
        assert!(
            after.iter().any(|(l, r)| l == "in" && *r == tier),
            "incoming agent must hold the outgoing role {tier:?}: {after:?}"
        );
        assert!(!after.iter().any(|(l, _)| l == "out"));
        // The roster never shrinks: one out, one in.
        assert_eq!(after.len(), 2, "{after:?}");
    }
}

#[test]
fn swapping_onto_a_label_already_on_the_roster_is_refused() {
    // The failure mode is SILENT: `agents` is keyed by label, so an
    // insert onto an existing one collapses two entries into one and
    // the gate's expected reviewer set shrinks by one. Assert on the
    // roster, not on the error.
    let env = TestEnv::init();
    common::register_team(env.home(), env.repo(), "claude", &["a", "b"], &[]);
    let before = roster(env.repo());

    let err = swap(&env, "a", "b").unwrap_err();
    assert!(
        err.to_string().contains("already on this repo's roster"),
        "{err}"
    );
    assert_eq!(roster(env.repo()), before, "nothing may be written");
}

#[test]
fn swapping_out_an_agent_that_is_not_on_the_roster_is_refused() {
    let env = TestEnv::init();
    common::register_team(env.home(), env.repo(), "claude", &["a"], &[]);
    declare(env.home(), "in");
    let before = roster(env.repo());

    let err = swap(&env, "ghost", "in").unwrap_err();
    assert!(err.to_string().contains("nothing to swap out"), "{err}");
    assert_eq!(roster(env.repo()), before, "nothing may be written");
}

#[test]
fn swapping_the_master_atomically_replaces_it() {
    let env = TestEnv::init();
    common::register_team(env.home(), env.repo(), "claude", &["a"], &[]);
    declare(env.home(), "in");

    swap(&env, "claude", "in").unwrap();

    let after = roster(env.repo());
    assert_eq!(after.len(), 2, "one agent out and one in: {after:?}");
    assert!(!after.iter().any(|(label, _)| label == "claude"));
    assert_eq!(
        after
            .iter()
            .filter(|(_, role)| *role == RosterRole::Master)
            .collect::<Vec<_>>(),
        vec![&("in".to_string(), RosterRole::Master)],
    );
    assert!(
        after
            .iter()
            .any(|(label, role)| { label == "a" && *role == RosterRole::Commit })
    );
}

#[test]
fn swapping_in_an_agent_the_library_does_not_know_is_refused() {
    let env = TestEnv::init();
    common::register_team(env.home(), env.repo(), "claude", &["a"], &[]);
    let before = roster(env.repo());

    let err = swap(&env, "a", "unknown").unwrap_err();
    assert!(err.to_string().contains("unknown agent"), "{err}");
    assert_eq!(roster(env.repo()), before, "nothing may be written");
}

fn git(repo: &Path, args: &[&str]) {
    let ok = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .status()
        .unwrap()
        .success();
    assert!(ok, "git {args:?}");
}

fn head_sha(repo: &Path) -> clank_core::ids::CommitSha {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    clank_core::ids::CommitSha::parse(String::from_utf8(out.stdout).unwrap().trim()).unwrap()
}

/// The gate as the repo's CURRENT roster sees it at `sha`.
fn gate_now(repo: &Path, sha: &clank_core::ids::CommitSha) -> clank_core::vocab::CommitGateState {
    use clank_core::wait::PlanStateLookup;
    let entries =
        clank::fs_plan_state_lookup::FsPlanStateLookup::new(repo, Some(sha)).reviews_for(sha);
    let tiers = clank::agent_store::load_reviewer_tiers(repo).unwrap();
    clank_core::wait::compute_gate(&entries, &tiers.commit, &tiers.plan, &tiers.final_, false)
}

#[tokio::test]
async fn a_departed_reviewers_verdict_does_not_satisfy_the_gate_for_its_replacement() {
    // A verdict is an AGENT's judgement, not the role's. The behaviour
    // holds by construction today — swap never touches feedback dirs —
    // but it is asserted HERE so that stays a decision rather than an
    // accident: copying feedback on swap, or loosening the stale
    // filter, would otherwise let `in` inherit an approval for code it
    // never read.
    use clank::cli::{FeedbackArgs, FeedbackCmd, FeedbackWriteArgs, VerdictArg};

    let env = TestEnv::init();
    common::register_team(env.home(), env.repo(), "claude", &["out"], &[]);
    declare(env.home(), "in");
    let repo = env.repo();

    std::fs::create_dir_all(repo.join(".clank/plans")).unwrap();
    std::fs::write(repo.join(".clank/plans/foo.md"), "# foo\n").unwrap();
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "[foo] intro"]);
    let sha = head_sha(repo);

    clank::cli::feedback::run(FeedbackArgs {
        command: FeedbackCmd::Write(FeedbackWriteArgs {
            repo: Some(repo.to_path_buf()),
            commit: sha.as_str().to_string(),
            verdict: VerdictArg::Finished,
            author: "out".into(),
            message: "ship it".into(),
        }),
    })
    .await
    .unwrap();

    // Before the swap the lone reviewer's FINISHED carries the gate.
    assert_eq!(
        gate_now(repo, &sha),
        clank_core::vocab::CommitGateState::Finished,
        "precondition: the departing reviewer's verdict counts while it is on the roster"
    );

    swap(&env, "out", "in").unwrap();

    // After it, the gate owes a review from `in` — the departed
    // verdict is void, not inherited.
    assert_eq!(
        gate_now(repo, &sha),
        clank_core::vocab::CommitGateState::Unreviewed,
        "the incoming reviewer owes a fresh review"
    );
}

#[tokio::test]
async fn replacing_the_master_does_not_rewind_a_settled_reviewer_gate() {
    use clank::cli::{FeedbackArgs, FeedbackCmd, FeedbackWriteArgs, VerdictArg};

    let env = TestEnv::init();
    common::register_team(env.home(), env.repo(), "master", &["rev"], &[]);
    declare(env.home(), "incoming-master");
    let repo = env.repo();

    std::fs::create_dir_all(repo.join(".clank/plans")).unwrap();
    std::fs::write(repo.join(".clank/plans/foo.md"), "# foo\n").unwrap();
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "[foo] intro"]);
    let sha = head_sha(repo);
    clank::cli::feedback::run(FeedbackArgs {
        command: FeedbackCmd::Write(FeedbackWriteArgs {
            repo: Some(repo.to_path_buf()),
            commit: sha.as_str().to_string(),
            verdict: VerdictArg::Finished,
            author: "rev".into(),
            message: "ship it".into(),
        }),
    })
    .await
    .unwrap();
    let before = gate_now(repo, &sha);

    swap(&env, "master", "incoming-master").unwrap();

    assert_eq!(gate_now(repo, &sha), before);
    assert_eq!(before, clank_core::vocab::CommitGateState::Finished);
}
