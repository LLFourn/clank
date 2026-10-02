//! In-process test: the status snapshot carries recent oneline log
//! lines for the TUI's live-log pane (status-tui-live-log).

use crate::common;

use common::TestEnv;
use std::path::Path;
use std::process::Command;

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

#[test]
fn snapshot_carries_recent_oneline_log_with_reviews() {
    let env = TestEnv::init();
    env.register_team("claude", &["codex"], &[]);
    let repo = env.repo();
    write(repo, ".clank/.gitignore", "/cache/\n/agents/\n/shelved/\n");
    write(repo, ".clank/plans/foo.md", "# foo\n");
    commit(repo, "[foo] intro");
    let intro = {
        let out = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    };
    write(
        repo,
        &format!(".clank/agents/codex/feedback/{intro}.md"),
        "CONTINUE lgtm\n",
    );
    write(repo, "src/a.rs", "// a\n");
    commit(repo, "[foo] impl a");

    let snap = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(clank::cli::status::snapshot(repo, Some(env.home())))
        .unwrap();
    let lines = snap.log_lines_for_test();
    let joined = lines.join("\n");
    // Umbrella shape (log-plan-umbrellas): plan header at col 0,
    // commits beneath with the [foo] prefix STRIPPED.
    assert!(
        lines.iter().any(|l| l == "foo"),
        "umbrella header:\n{joined}"
    );
    assert!(!joined.contains("[foo]"), "prefix stripped:\n{joined}");
    assert!(joined.contains(" intro"), "got:\n{joined}");
    assert!(joined.contains(" impl a"), "got:\n{joined}");
    assert!(
        joined.contains("✓ codex: lgtm"),
        "review sub-line:\n{joined}"
    );
    // Chronological: intro before impl.
    assert!(
        joined.find(" impl a").unwrap() < joined.find(" intro").unwrap(),
        "newest first:\n{joined}"
    );
}

#[test]
fn legacy_cache_rebuild_and_warm_resume_share_one_accepted_plan() {
    let env = TestEnv::init();
    env.register_team("master", &["reviewer"], &[]);
    let repo = env.repo();
    write(
        repo,
        ".clank/.gitignore",
        "/cache/\n/agents/\n/config.json\n/shelved/\n",
    );
    write(repo, ".clank/plans/accepted.md", "# accepted\n");
    commit(repo, "[accepted] intro");
    write(repo, ".clank/plans/overlapping.md", "# overlapping\n");
    commit(repo, "[overlapping] intro");
    let head = clank::git_io::rev_parse_head(repo).unwrap().unwrap();
    // Keep the genuinely old map payload; only adapt its envelope to this
    // fixture's real HEAD, just as an old writer would name the checkpoint.
    let mut old = include_bytes!("fixtures/repo-state-v13.bin").to_vec();
    let head_offset = b"CLANK-STATE\n".len() + 8;
    old[head_offset..head_offset + 40].copy_from_slice(head.as_str().as_bytes());
    let cache = repo.join(".clank/cache/repo-state");
    std::fs::create_dir_all(&cache).unwrap();
    let old_path = cache.join(format!("{}.2.v13.bin", head.as_str()));
    std::fs::write(&old_path, &old).unwrap();

    let rt = tokio::runtime::Runtime::new().unwrap();
    let snapshot = || {
        rt.block_on(clank::cli::status::snapshot(repo, Some(env.home())))
            .unwrap()
    };
    let cold = snapshot();
    let json = cold.to_json();
    assert!(json["active_plan"].is_object());
    assert_eq!(json["active_plan"]["plan"], "accepted");
    assert_eq!(
        json["warnings"][0]["warning"]["kind"],
        "rejected_plan_intro"
    );
    assert!(json.get("head_correction").is_none());
    assert!(
        cold.to_human()
            .contains("ignored introduction of `overlapping`")
    );
    assert_eq!(
        std::fs::read(&old_path).unwrap(),
        old,
        "old checkpoint preserved"
    );
    assert!(std::fs::read_dir(&cache).unwrap().any(|e| {
        e.unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".v15.bin")
    }));
    assert_eq!(
        snapshot().to_json(),
        json,
        "warm cache agrees with the cold fold"
    );

    write(repo, ".clank/finished/accepted.md", "# accepted\n");
    std::fs::remove_file(repo.join(".clank/plans/accepted.md")).unwrap();
    commit(repo, "[accepted] finish");
    write(repo, "src.rs", "// ignored implementation\n");
    commit(repo, "[overlapping] implementation");
    let after = snapshot().to_json();
    assert!(after["active_plan"].is_null());
    assert_eq!(after["last_finished"]["plan"], "accepted");
    assert!(after.get("head_correction").is_none());
    assert!(
        after.get("ad_hoc").is_none(),
        "ignored tags do not create reviewer work: {after}"
    );
    assert_eq!(after["warnings"].as_array().unwrap().len(), 1);
}

#[test]
fn v14_finished_history_refolds_duration_once_and_reuses_the_v15_checkpoint() {
    let env = TestEnv::init();
    env.register_team("master", &["reviewer"], &[]);
    let repo = env.repo();
    write(
        repo,
        ".clank/.gitignore",
        "/cache/\n/agents/\n/config.json\n/shelved/\n/queue/\n",
    );
    let dated_commit = |ts: i64, subject: &str| {
        git(repo, &["add", "-A"]);
        let date = format!("@{ts} +0000");
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(repo)
                .args(["commit", "--quiet", "-m", subject])
                .env("GIT_AUTHOR_DATE", &date)
                .env("GIT_COMMITTER_DATE", &date)
                .status()
                .unwrap()
                .success()
        );
    };
    write(repo, ".clank/plans/first.md", "# first\n");
    dated_commit(1_700_000_000, "[first] intro");
    write(repo, "src.rs", "// implementation\n");
    dated_commit(1_700_000_030, "[first] implementation");
    write(repo, ".clank/finished/first.md", "# first\n");
    std::fs::remove_file(repo.join(".clank/plans/first.md")).unwrap();
    dated_commit(1_700_000_120, "[first] finish");
    write(repo, ".clank/plans/next.md", "# next\n");
    dated_commit(1_700_000_200, "[next] intro");
    let head = clank::git_io::rev_parse_head(repo).unwrap().unwrap();
    let mut old = include_bytes!("fixtures/repo-state-v14.bin").to_vec();
    let offset = b"CLANK-STATE\n".len() + 8;
    old[offset..offset + 40].copy_from_slice(head.as_str().as_bytes());
    let cache = repo.join(".clank/cache/repo-state");
    std::fs::create_dir_all(&cache).unwrap();
    let legacy = cache.join(format!("{}.4.v14.bin", head.as_str()));
    std::fs::write(&legacy, &old).unwrap();
    let queue = repo.join(".clank/queue/later.md");
    write(repo, ".clank/queue/later.md", "# untouched\n");
    let binding = repo.join(".clank/config.json");
    let config_before = std::fs::read(&binding).unwrap();
    let cold =
        clank::rebuild::rebuild_repo_sync_with_policy(repo, clank::rebuild::CachePolicy::Use)
            .unwrap();
    assert_eq!(cold.fold.active_plan.as_ref().unwrap().key.as_str(), "next");
    assert_eq!(cold.fold.finished_plans[0].duration_secs, 120);
    let current = std::fs::read_dir(&cache)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(head.as_str())
                && p.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .ends_with(".v15.bin")
        })
        .unwrap();
    let bytes = std::fs::read(&current).unwrap();
    let mtime = std::fs::metadata(&current).unwrap().modified().unwrap();
    let warm =
        clank::rebuild::rebuild_repo_sync_with_policy(repo, clank::rebuild::CachePolicy::Use)
            .unwrap();
    assert_eq!(warm.fold, cold.fold);
    assert_eq!(std::fs::read(&current).unwrap(), bytes);
    assert_eq!(
        std::fs::metadata(current).unwrap().modified().unwrap(),
        mtime
    );
    assert_eq!(std::fs::read(legacy).unwrap(), old);
    assert_eq!(clank::git_io::rev_parse_head(repo).unwrap().unwrap(), head);
    assert_eq!(std::fs::read_to_string(queue).unwrap(), "# untouched\n");
    assert_eq!(std::fs::read(binding).unwrap(), config_before);
}
