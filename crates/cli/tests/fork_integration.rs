//! In-process tests for `clank fork` (clank-fork-worktree-sessions).
//! Drives `fork::run_fork` directly — no binary spawning; the open
//! decision is pure-tested in the unit suite.

mod common;

use common::TestEnv;
use std::path::Path;
use std::process::Command;

fn git_out(repo: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .expect("git");
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn git(repo: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .status()
        .expect("git");
    assert!(status.success(), "git {args:?} failed");
}

fn write(repo: &Path, rel: &str, body: &str) {
    let abs = repo.join(rel);
    std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
    std::fs::write(abs, body).unwrap();
}

fn commit(repo: &Path, msg: &str) {
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", msg]);
}

fn bind_session(env: &TestEnv, label: &str, tool: clank_core::vocab::Tool, id: &str) {
    let l = clank_core::ids::AgentLabel::parse(label).unwrap();
    let cfg = clank_core::agent_config::AgentConfig {
        auto_mode: None,
        wait_timeout: None,
        session: Some(clank_core::agent_config::Session {
            id: clank_core::ids::SessionId::parse(id).unwrap(),
            tool,
            updated_at: "2026-06-10T12:00:00Z".to_string(),
        }),
    };
    clank::agent_store::save_agent_config(env.repo(), &l, &cfg).unwrap();
}

/// Source repo: claude master + codex commit reviewer, both with
/// bound sessions, one commit so worktrees have a base.
fn source_with_bound_team() -> TestEnv {
    let env = TestEnv::init();
    env.register_team("claude", &["codex"], &[]);
    let repo = env.repo();
    write(
        repo,
        ".clank/.gitignore",
        &clank::init_facts::clank_gitignore_body(),
    );
    write(repo, "src/lib.rs", "// base\n");
    commit(repo, "[misc] base");
    bind_session(
        &env,
        "claude",
        clank_core::vocab::Tool::Claude,
        "11111111-1111-1111-1111-111111111111",
    );
    bind_session(
        &env,
        "codex",
        clank_core::vocab::Tool::Codex,
        "22222222-2222-2222-2222-222222222222",
    );
    env
}

fn fork_args(env: &TestEnv, name: &str) -> clank::cli::ForkArgs {
    clank::cli::ForkArgs {
        name: Some(name.into()),
        source: Some(env.repo().to_path_buf()),
        pr: None,
        branch: None,
        path: None,
        drafts: Vec::new(),
        prompt: None,
        no_open: true,
        review: false,
    }
}

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Runtime::new().unwrap().block_on(f)
}

#[test]
fn fork_creates_worktree_and_seeds_team() {
    let env = source_with_bound_team();
    let repo = env.repo();

    let dest = block_on(clank::cli::fork::run_fork(
        &fork_args(&env, "myfeature"),
        Some(env.home()),
    ))
    .unwrap();
    // resolve_repo canonicalizes (/var → /private/var on macOS);
    // compare canonicalized forms.
    assert_eq!(
        dest.canonicalize().unwrap(),
        repo.join(".clank/worktrees/myfeature")
            .canonicalize()
            .unwrap()
    );
    assert!(dest.is_dir());

    // New branch named after the fork, based on source HEAD.
    let branch = git_out(&dest, &["rev-parse", "--abbrev-ref", "HEAD"]);
    assert_eq!(branch.trim(), "myfeature");
    assert_eq!(
        git_out(&dest, &["rev-parse", "HEAD"]).trim(),
        git_out(repo, &["rev-parse", "HEAD"]).trim(),
        "branched off source HEAD"
    );

    // Seeded: repo config (team selection) copied; tracked state
    // arrives via the checkout.
    assert_eq!(
        std::fs::read_to_string(dest.join(".clank/config.json")).unwrap(),
        std::fs::read_to_string(repo.join(".clank/config.json")).unwrap(),
        "team selection seeded"
    );
    assert!(
        dest.join(".clank/.gitignore").is_file(),
        "tracked state checked out"
    );
    // The default worktree location never pollutes main-repo status:
    // the allow-list gitignore (`/*`) covers `.clank/worktrees/` with
    // no per-command help.
    assert_eq!(
        git_out(repo, &["status", "--porcelain"]).trim(),
        "",
        "fork must leave main-repo status clean"
    );

    // Per-agent fork specs with the right source ids + orientation.
    for (label, id_prefix) in [("claude", "11111111"), ("codex", "22222222")] {
        let spec: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(dest.join(format!(".clank/agents/{label}/fork.json")))
                .unwrap(),
        )
        .unwrap();
        assert!(
            spec["from_session"]
                .as_str()
                .unwrap()
                .starts_with(id_prefix),
            "{label} forks from its source session"
        );
        let prompt = spec["prompt"].as_str().unwrap();
        assert!(
            prompt.contains("worktree `myfeature`"),
            "orientation: {prompt}"
        );
        assert!(
            prompt.contains(&format!("clank as {label}")),
            "bind instruction: {prompt}"
        );
    }
}

#[test]
fn fork_carbon_copies_per_agent_settings_not_session() {
    use clank_core::vocab::AutoMode;
    let env = source_with_bound_team();
    let repo = env.repo();
    let claude = clank_core::ids::AgentLabel::parse("claude").unwrap();

    // Source claude: explicit auto OFF + a wait_timeout (on top of its
    // bound session).
    let mut src = clank::agent_store::load_agent_config(repo, &claude)
        .unwrap()
        .unwrap();
    src.auto_mode = Some(AutoMode::Off);
    src.wait_timeout = Some("5m".into());
    clank::agent_store::save_agent_config(repo, &claude, &src).unwrap();

    let dest = block_on(clank::cli::fork::run_fork(
        &fork_args(&env, "wt"),
        Some(env.home()),
    ))
    .unwrap();

    // Settings carbon-copied; session NOT (the fork mints its own).
    let dst = clank::agent_store::load_agent_config(&dest, &claude)
        .unwrap()
        .unwrap();
    assert_eq!(dst.auto_mode, Some(AutoMode::Off), "explicit auto carried");
    assert_eq!(dst.wait_timeout.as_deref(), Some("5m"), "timeout carried");
    assert!(dst.session.is_none(), "session is NOT copied");

    // codex (source auto unset, no timeout) → nothing to carry, so no
    // dest config is written; the fork inherits the global default.
    let codex = clank_core::ids::AgentLabel::parse("codex").unwrap();
    assert!(
        clank::agent_store::load_agent_config(&dest, &codex)
            .unwrap()
            .is_none(),
        "unset source carries nothing (inherits global default)"
    );

    // `clank as` binding a NEW session MERGES — the carbon-copied
    // settings survive.
    let new_sid =
        clank_core::ids::SessionId::parse("33333333-3333-3333-3333-333333333333").unwrap();
    clank::agent_store::bind_session_to_agent(
        &dest,
        &claude,
        clank_core::vocab::Tool::Claude,
        &new_sid,
    )
    .unwrap();
    let after = clank::agent_store::load_agent_config(&dest, &claude)
        .unwrap()
        .unwrap();
    assert_eq!(after.auto_mode, Some(AutoMode::Off), "auto survives bind");
    assert_eq!(
        after.wait_timeout.as_deref(),
        Some("5m"),
        "timeout survives bind"
    );
    assert_eq!(after.session.unwrap().id, new_sid, "new session bound");
}

#[test]
fn fork_from_worktree_lands_sibling_under_main_not_nested() {
    let env = source_with_bound_team();
    let main = env.repo().to_path_buf();

    // B: fork from main.
    let b = block_on(clank::cli::fork::run_fork(
        &fork_args(&env, "wt1"),
        Some(env.home()),
    ))
    .unwrap();
    assert_eq!(
        b.canonicalize().unwrap(),
        main.join(".clank/worktrees/wt1").canonicalize().unwrap()
    );

    // Bind sessions IN B (simulate `clank as` in the worktree) so it
    // can itself be forked.
    for (label, tool, id) in [
        (
            "claude",
            clank_core::vocab::Tool::Claude,
            "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
        ),
        (
            "codex",
            clank_core::vocab::Tool::Codex,
            "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb",
        ),
    ] {
        let l = clank_core::ids::AgentLabel::parse(label).unwrap();
        let cfg = clank_core::agent_config::AgentConfig {
            auto_mode: None,
            wait_timeout: None,
            session: Some(clank_core::agent_config::Session {
                id: clank_core::ids::SessionId::parse(id).unwrap(),
                tool,
                updated_at: "2026-06-17T00:00:00Z".to_string(),
            }),
        };
        clank::agent_store::save_agent_config(&b, &l, &cfg).unwrap();
    }

    // C: fork FROM B.
    let mut from_b = fork_args(&env, "wt2");
    from_b.source = Some(b.clone());
    let c = block_on(clank::cli::fork::run_fork(&from_b, Some(env.home()))).unwrap();

    // C is a SIBLING under main, NOT nested under B.
    assert_eq!(
        c.canonicalize().unwrap(),
        main.join(".clank/worktrees/wt2").canonicalize().unwrap(),
        "fork-from-worktree must land under the MAIN repo"
    );
    assert!(
        !c.starts_with(&b),
        "C must not be nested under B: {}",
        c.display()
    );
    assert_eq!(
        c.to_string_lossy().matches(".clank/worktrees").count(),
        1,
        "exactly one worktrees segment (no nesting): {}",
        c.display()
    );
}

#[test]
fn fork_refuses_when_sessions_missing() {
    let env = TestEnv::init();
    env.register_team("claude", &["codex"], &[]);
    let repo = env.repo();
    write(repo, "src/lib.rs", "// base\n");
    commit(repo, "[misc] base");
    // Only claude bound; codex has no session.
    bind_session(
        &env,
        "claude",
        clank_core::vocab::Tool::Claude,
        "11111111-1111-1111-1111-111111111111",
    );

    let err = block_on(clank::cli::fork::run_fork(
        &fork_args(&env, "x"),
        Some(env.home()),
    ))
    .unwrap_err()
    .to_string();
    assert!(err.contains("codex"), "names the unbound agent: {err}");
    assert!(
        !repo.join(".clank/worktrees/x").exists(),
        "fail-closed: nothing created"
    );
}

#[test]
fn fork_respects_branch_and_path() {
    let env = source_with_bound_team();
    let repo = env.repo();
    let base = git_out(repo, &["rev-parse", "HEAD"]);
    write(repo, "src/more.rs", "// more\n");
    commit(repo, "[misc] newer");

    let dest_override = repo.join("custom-wt");
    let mut args = fork_args(&env, "offbase");
    args.branch = Some(base.trim().to_string());
    args.path = Some(dest_override.clone());
    let dest = block_on(clank::cli::fork::run_fork(&args, Some(env.home()))).unwrap();
    assert_eq!(
        dest.canonicalize().unwrap(),
        dest_override.canonicalize().unwrap()
    );
    assert_eq!(
        git_out(&dest, &["rev-parse", "HEAD"]).trim(),
        base.trim(),
        "--branch base respected (not HEAD)"
    );
}

#[test]
fn fork_refuses_existing_destination() {
    let env = source_with_bound_team();
    let repo = env.repo();
    std::fs::create_dir_all(repo.join(".clank/worktrees/taken")).unwrap();
    let err = block_on(clank::cli::fork::run_fork(
        &fork_args(&env, "taken"),
        Some(env.home()),
    ))
    .unwrap_err()
    .to_string();
    assert!(err.contains("already exists"), "got: {err}");
}

#[test]
fn fork_rejects_bad_names() {
    let env = source_with_bound_team();
    for bad in ["a/b", "has space", ""] {
        let err = block_on(clank::cli::fork::run_fork(
            &fork_args(&env, bad),
            Some(env.home()),
        ))
        .unwrap_err()
        .to_string();
        assert!(err.contains("simple"), "`{bad}` rejected: {err}");
    }
}

#[test]
fn fork_worktree_ignored_by_allow_list_without_touching_gitignore() {
    // Under the single-source allow-list (the harness writes it), the
    // `.clank/worktrees/` dir is ignored by `/*` — so fork keeps `git status`
    // clean WITHOUT appending anything (the old per-dir self-heal is gone).
    let env = source_with_bound_team();
    let repo = env.repo();
    let before = std::fs::read_to_string(repo.join(".clank/.gitignore")).unwrap();

    block_on(clank::cli::fork::run_fork(
        &fork_args(&env, "wt"),
        Some(env.home()),
    ))
    .unwrap();

    let status = git_out(repo, &["status", "--porcelain"]);
    assert!(
        !status.contains("worktrees"),
        "worktree dir must be ignored by /*: {status}"
    );
    let after = std::fs::read_to_string(repo.join(".clank/.gitignore")).unwrap();
    assert_eq!(
        after, before,
        "fork must not touch the allow-list gitignore"
    );
}

/// A bare "origin" carrying refs/pull/123/head, so `fork --pr`
/// exercises the real fetch path with no network and no gh (the
/// title lookup fails -> the degraded prompt, deterministically).
fn add_local_pr_remote(env: &TestEnv, pr_head_msg: &str) -> String {
    let repo = env.repo();
    // Inside this env's own tempdir (home) — a shared-/tmp path
    // collides under parallel test runs.
    let bare = env.home().join("origin.git");
    git(repo, &["init", "--bare", "--quiet", bare.to_str().unwrap()]);
    git(repo, &["remote", "add", "origin", bare.to_str().unwrap()]);
    // A divergent PR head: commit on a temp branch, push to the
    // pull ref, then drop the local branch.
    git(repo, &["checkout", "--quiet", "-b", "tmp-pr"]);
    write(repo, "src/pr_change.rs", "// pr\n");
    commit(repo, pr_head_msg);
    let sha = git_out(repo, &["rev-parse", "HEAD"]).trim().to_string();
    git(
        repo,
        &["push", "--quiet", "origin", "HEAD:refs/pull/123/head"],
    );
    git(repo, &["checkout", "--quiet", "-"]);
    git(repo, &["branch", "--quiet", "-D", "tmp-pr"]);
    sha
}

/// Like `add_local_pr_remote`, but the bare origin lives at a
/// `github.com/<owner>/<name>.git` path so `repo_slug` parses
/// `LLFourn/clank` from it while the fetch still works offline —
/// `--review` needs the slug for the github API.
fn add_local_pr_remote_gh(env: &TestEnv, pr_head_msg: &str) -> String {
    let repo = env.repo();
    let bare = env.home().join("github.com/LLFourn/clank.git");
    std::fs::create_dir_all(bare.parent().unwrap()).unwrap();
    git(repo, &["init", "--bare", "--quiet", bare.to_str().unwrap()]);
    git(repo, &["remote", "add", "origin", bare.to_str().unwrap()]);
    git(repo, &["checkout", "--quiet", "-b", "tmp-pr"]);
    write(repo, "src/pr_change.rs", "// pr\n");
    commit(repo, pr_head_msg);
    let sha = git_out(repo, &["rev-parse", "HEAD"]).trim().to_string();
    git(
        repo,
        &["push", "--quiet", "origin", "HEAD:refs/pull/123/head"],
    );
    git(repo, &["checkout", "--quiet", "-"]);
    git(repo, &["branch", "--quiet", "-D", "tmp-pr"]);
    sha
}

#[test]
fn fork_pr_review_scaffolds_review_in_the_worktree() {
    // The shared core behind `fork --pr --review` AND `pr-review
    // start --fork`: worktree on the PR head + review scaffold in
    // the worktree's .clank/, with the slug from the source origin.
    let env = source_with_bound_team();
    let pr_sha = add_local_pr_remote_gh(&env, "[misc] pr change");

    let mut args = fork_args(&env, "ignored");
    args.name = None;
    args.pr = Some(123);
    args.review = true;
    let dest = block_on(clank::cli::fork::run_fork_with_review(
        &args,
        Some(env.home()),
    ))
    .unwrap();

    let pr_json = dest.join(".clank/pr-reviews/123/pr.json");
    assert!(pr_json.is_file(), "review scaffolded in the worktree");
    let state: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&pr_json).unwrap()).unwrap();
    assert_eq!(state["number"], 123);
    assert_eq!(state["repo"], "LLFourn/clank", "slug from source origin");
    assert_eq!(state["round"], 0, "round 0 = master drafting");
    // Pin-once invariant (ruthless b88ae34): the review anchors to the
    // SAME sha the worktree is based on — by construction, one fetch.
    assert_eq!(state["head_sha"], pr_sha, "review pins the worktree's head");
    assert_eq!(
        git_out(&dest, &["rev-parse", "HEAD"]).trim(),
        pr_sha,
        "worktree based on that same head"
    );
}

#[test]
fn fork_review_validates_slug_before_creating_the_worktree() {
    // origin is a local path that does NOT parse as a github slug,
    // but the PR head IS fetchable. Without the preflight, run_fork
    // would create the worktree and THEN repo_slug would fail,
    // orphaning it (codex b944c58). Fail-closed: no worktree.
    let env = source_with_bound_team();
    add_local_pr_remote(&env, "[misc] pr change"); // origin.git — not github-shaped
    let mut args = fork_args(&env, "ignored");
    args.name = None;
    args.pr = Some(123);
    args.review = true;

    let err = block_on(clank::cli::fork::run_fork_with_review(
        &args,
        Some(env.home()),
    ))
    .unwrap_err()
    .to_string();
    assert!(err.contains("owner/name"), "slug parse error: {err}");
    assert!(
        !env.repo().join(".clank/worktrees/pr-123").exists(),
        "fail-closed: slug validated before the worktree is created"
    );
}

#[test]
fn fork_pr_fetches_pins_and_orients() {
    let env = source_with_bound_team();
    let pr_sha = add_local_pr_remote(&env, "[misc] the pr change");

    let mut args = fork_args(&env, "ignored");
    args.name = None;
    args.pr = Some(123);
    let dest = block_on(clank::cli::fork::run_fork(&args, Some(env.home()))).unwrap();

    // Name derived, worktree based on the PINNED PR head sha.
    assert!(dest.ends_with(".clank/worktrees/pr-123"), "{dest:?}");
    assert_eq!(git_out(&dest, &["rev-parse", "HEAD"]).trim(), pr_sha);
    // Orientation: degraded gh-less prompt, deterministic.
    let spec = std::fs::read_to_string(dest.join(".clank/agents/claude/fork.json")).unwrap();
    assert!(
        spec.contains("reviewing PR #123"),
        "PR orientation in prompt: {spec}"
    );
}

#[test]
fn fork_pr_precondition_failure_never_touches_network() {
    // Ruthless 91ecaf2 edge 1: the fetch sits AFTER the fail-closed
    // line. With an unbound session AND an invalid remote, the
    // error must be the session one — a fetch attempt would have
    // failed loudly with a fetch error instead.
    let env = TestEnv::init();
    env.register_team("claude", &["codex"], &[]);
    let repo = env.repo();
    write(repo, "src/lib.rs", "// base\n");
    commit(repo, "[misc] base");
    git(
        repo,
        &["remote", "add", "origin", "/nonexistent/nowhere.git"],
    );
    bind_session(
        &env,
        "claude",
        clank_core::vocab::Tool::Claude,
        "11111111-1111-1111-1111-111111111111",
    );
    // codex unbound.
    let mut args = fork_args(&env, "ignored");
    args.name = None;
    args.pr = Some(123);
    let err = block_on(clank::cli::fork::run_fork(&args, Some(env.home())))
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("codex") && err.contains("bound session"),
        "precondition error, not a fetch error: {err}"
    );
}

// ── fork-draft-seeding ──

#[test]
fn fork_moves_drafts_into_the_queue_in_list_order() {
    let env = source_with_bound_team();
    let repo = env.repo();
    write(repo, ".clank/drafts/first.md", "# first\n\nbody one\n");
    write(repo, ".clank/drafts/second.md", "# second\n\nbody two\n");

    let mut args = fork_args(&env, "seeded");
    // `.md` suffix optional per name.
    args.drafts = vec!["first".into(), "second.md".into()];
    let dest = block_on(clank::cli::fork::run_fork(&args, Some(env.home()))).unwrap();

    // First named draft gets the lowest priority; canonical names.
    let q = dest.join(".clank/queue");
    let one = std::fs::read_to_string(q.join("000-first.md")).unwrap();
    let two = std::fs::read_to_string(q.join("001-second.md")).unwrap();
    assert!(one.contains("body one"));
    assert!(two.contains("body two"));

    // Moved, not copied: the source drafts are consumed.
    assert!(!repo.join(".clank/drafts/first.md").exists());
    assert!(!repo.join(".clank/drafts/second.md").exists());

    // Orientation names the seeded queue in order.
    let spec: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(dest.join(".clank/agents/claude/fork.json")).unwrap(),
    )
    .unwrap();
    let prompt = spec["prompt"].as_str().unwrap();
    assert!(
        prompt.contains("in order: first, second"),
        "orientation mentions the seeded queue: {prompt}"
    );
}

#[test]
fn fork_with_a_missing_draft_fails_before_any_mutation() {
    let env = source_with_bound_team();
    let repo = env.repo();
    write(repo, ".clank/drafts/real.md", "# real\n\nbody\n");

    let mut args = fork_args(&env, "typo");
    args.drafts = vec!["real".into(), "no-such-draft".into()];
    let err = block_on(clank::cli::fork::run_fork(&args, Some(env.home()))).unwrap_err();
    assert!(err.to_string().contains("no-such-draft"), "{err}");

    // Fail-fast: no worktree, and the real draft was not consumed.
    assert!(!repo.join(".clank/worktrees/typo").exists());
    assert!(repo.join(".clank/drafts/real.md").is_file());
}

#[test]
fn refork_refuses_a_draft_colliding_with_an_already_seeded_entry() {
    // The queue is gitignored per-worktree state, so a FRESH fork's
    // queue starts empty — a collision can only arise on re-fork: a
    // stem seeded by a previous run whose draft was since re-created.
    let env = source_with_bound_team();
    let repo = env.repo();
    write(repo, ".clank/drafts/taken.md", "# taken\n\nqueued body\n");
    let mut args = fork_args(&env, "collide");
    args.drafts = vec!["taken".into()];
    block_on(clank::cli::fork::run_fork(&args, Some(env.home()))).unwrap();

    // Re-create the consumed draft, add a second, re-fork with both.
    write(repo, ".clank/drafts/taken.md", "# taken\n\nnew body\n");
    write(repo, ".clank/drafts/other.md", "# other\n\nother body\n");
    args.drafts = vec!["other".into(), "taken".into()];
    let err = block_on(clank::cli::fork::run_fork(&args, Some(env.home()))).unwrap_err();
    assert!(err.to_string().contains("taken"), "{err}");

    // Checked for the WHOLE list before any move: neither draft moved.
    assert!(repo.join(".clank/drafts/taken.md").is_file());
    assert!(repo.join(".clank/drafts/other.md").is_file());
}

#[test]
fn refork_seeds_remaining_drafts_and_skips_already_queued() {
    let env = source_with_bound_team();
    let repo = env.repo();
    write(repo, ".clank/drafts/alpha.md", "# alpha\n\na body\n");

    let mut args = fork_args(&env, "retry");
    args.drafts = vec!["alpha".into()];
    let dest = block_on(clank::cli::fork::run_fork(&args, Some(env.home()))).unwrap();
    assert!(dest.join(".clank/queue/000-alpha.md").is_file());

    // Re-fork with alpha (consumed, already queued) + a new beta:
    // alpha counts as seeded, beta lands at its list position.
    write(repo, ".clank/drafts/beta.md", "# beta\n\nb body\n");
    args.drafts = vec!["alpha".into(), "beta".into()];
    let dest2 = block_on(clank::cli::fork::run_fork(&args, Some(env.home()))).unwrap();
    assert_eq!(dest2, dest);
    assert!(dest.join(".clank/queue/000-alpha.md").is_file());
    assert!(dest.join(".clank/queue/001-beta.md").is_file());
    assert!(!repo.join(".clank/drafts/beta.md").exists());
}

#[test]
fn fork_rejects_an_invalid_draft_name_before_any_mutation() {
    // codex 82f3e9c: `--draft foo/bar` whose NESTED drafts file exists
    // passes a bare existence check but is not a valid plan name — it
    // must fail in preflight, never after the worktree is created.
    let env = source_with_bound_team();
    let repo = env.repo();
    write(repo, ".clank/drafts/foo/bar.md", "# bar\n\nnested body\n");

    let mut args = fork_args(&env, "badname");
    args.drafts = vec!["foo/bar".into()];
    let err = block_on(clank::cli::fork::run_fork(&args, Some(env.home()))).unwrap_err();
    assert!(err.to_string().contains("foo/bar"), "{err}");

    assert!(
        !repo.join(".clank/worktrees/badname").exists(),
        "no worktree on an invalid draft name"
    );
    assert!(
        repo.join(".clank/drafts/foo/bar.md").is_file(),
        "nothing consumed"
    );
}
