//! `clank purge <plan> --drop`: delete a plan's commits and its `.clank/`
//! artifacts from history, wherever they are, keeping every other commit.
//!
//! One [`DropProgram`], built from the rewrite preview, is what every
//! executor carries out: the in-process replay (the live default and
//! `--dry`'s assessment), and the `git rebase --interactive` it hands
//! over to when a kept commit genuinely conflicts.

use std::path::Path;

use clank_core::api::RewritePreviewResponse;
use clank_core::ids::CommitSha;

use crate::git_plumbing::{self, ExpectedRef, RebaseStep, Rebased, Replayed};

#[derive(Debug, Clone)]
pub(crate) struct DropStep {
    pub sha: String,
    pub subject: String,
    /// Not the plan's: replayed as its change, and always kept as a
    /// commit, however little of it survives.
    pub keep: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct DropProgram {
    pub stem: String,
    /// The parent of the plan's first commit; `None` when that commit is
    /// the root, so the rewrite starts from the empty tree.
    pub base: Option<String>,
    /// The tip the preview was built from.
    pub head_sha: String,
    /// The branch HEAD was attached to then; `None` when detached.
    pub branch: Option<String>,
    pub linear: bool,
    pub steps: Vec<DropStep>,
    /// The dropped plan's own artifacts, removed from every kept commit.
    pub artifact_paths: Vec<String>,
}

/// What replaying the program in the object database came to. Nothing
/// it writes is referenced by any ref.
#[derive(Debug)]
pub(crate) enum Assessed {
    Clean {
        tip: String,
        pairs: Vec<(CommitSha, CommitSha)>,
    },
    Conflict {
        sha: String,
        subject: String,
        paths: Vec<String>,
    },
}

impl DropProgram {
    pub(crate) fn from_preview(
        repo: &Path,
        preview: &RewritePreviewResponse,
    ) -> anyhow::Result<Self> {
        let stem = preview.plan_stem.as_str().to_string();
        let intro = preview
            .intro_sha
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("plan `{stem}` has no commits to drop"))?;
        Ok(Self {
            base: crate::git_io::first_parent_at(repo, intro.as_str())?,
            head_sha: preview.head_sha.as_str().to_string(),
            branch: crate::git_io::current_branch_at(repo)?,
            linear: preview.linear,
            steps: preview
                .commits
                .iter()
                .map(|c| DropStep {
                    sha: c.sha.as_str().to_string(),
                    subject: c.subject.clone(),
                    keep: c.foreign,
                })
                .collect(),
            artifact_paths: vec![
                crate::init_facts::plan_md_rel(&stem),
                crate::init_facts::finished_md_rel(&stem),
            ],
            stem,
        })
    }

    pub(crate) fn dropped(&self) -> usize {
        self.steps.iter().filter(|s| !s.keep).count()
    }

    pub(crate) fn kept(&self) -> usize {
        self.steps.iter().filter(|s| s.keep).count()
    }

    fn leaves_nothing(&self) -> bool {
        self.base.is_none() && self.kept() == 0
    }

    /// The todo the git executor runs: the same steps, with each kept
    /// commit's artifact cleanup as an `exec`.
    pub(crate) fn todo(&self) -> String {
        let steps: Vec<RebaseStep<'_>> = self
            .steps
            .iter()
            .map(|s| match s.keep {
                true => RebaseStep::Pick {
                    sha: &s.sha,
                    subject: &s.subject,
                },
                false => RebaseStep::Drop {
                    sha: &s.sha,
                    subject: &s.subject,
                },
            })
            .collect();
        git_plumbing::drop_rebase_todo(&steps, &self.artifact_paths, self.base.is_none())
    }

    /// Replay the program in the object database: each kept commit's
    /// change onto the tree before it, minus the plan's artifacts, as a
    /// commit with its own message and author. A conflict is an answer,
    /// not an error; any other failure is.
    pub(crate) fn assess(&self, repo: &Path) -> anyhow::Result<Assessed> {
        let mut parent = self.base.clone();
        let mut tree = match &self.base {
            Some(base) => git_plumbing::commit_tree_oid(repo, base)?,
            None => git_plumbing::empty_tree(repo)?,
        };
        let mut pairs = Vec::new();
        for step in self.steps.iter().filter(|s| s.keep) {
            let replayed = match git_plumbing::replay_change(repo, &step.sha, &tree)? {
                Replayed::Tree(t) => t,
                Replayed::Conflicts(paths) => {
                    return Ok(Assessed::Conflict {
                        sha: step.sha.clone(),
                        subject: step.subject.clone(),
                        paths,
                    });
                }
            };
            tree = git_plumbing::strip_tree_id(repo, &replayed, &self.artifact_paths)?;
            let new = git_plumbing::replay_commit(repo, &step.sha, &tree, parent.as_deref())?;
            pairs.push((parse_sha(&step.sha)?, parse_sha(&new)?));
            parent = Some(new);
        }
        let tip = parent.ok_or_else(|| anyhow::anyhow!(nothing_left_message()))?;
        Ok(Assessed::Clean { tip, pairs })
    }

    /// Why this run must not start, checked before anything is written
    /// and again before a git rebase is handed the todo.
    fn blockers(&self, repo: &Path, into_branch: Option<&str>) -> anyhow::Result<Vec<String>> {
        let mut blockers = Vec::new();
        if let Some(op) = crate::git_io::operation_in_progress_at(repo)? {
            blockers.push(format!("a `{op}` is in progress; finish or abort it first"));
        }
        if crate::cli::rewrite::working_tree_dirty(repo)? {
            blockers.push("working tree dirty; commit or stash first".to_string());
        }
        if !self.linear {
            blockers.push(
                "range contains a merge commit; refusing to rewrite (merge-tree \
                 rewriting is out of scope)"
                    .to_string(),
            );
        }
        if self.leaves_nothing() {
            blockers.push(nothing_left_message().to_string());
        }
        let head = crate::git_io::rev_parse_head(repo)?.map(|s| s.as_str().to_string());
        if head.as_deref() != Some(self.head_sha.as_str()) {
            blockers.push(format!(
                "HEAD moved since the plan was read (expected {}, now {}); nothing \
                 was rewritten — re-run to refresh",
                short(&self.head_sha),
                head.as_deref()
                    .map(short)
                    .unwrap_or_else(|| "nothing".into()),
            ));
        }
        let branch = crate::git_io::current_branch_at(repo)?;
        if branch != self.branch {
            blockers.push(format!(
                "HEAD is no longer on {} (now {}); nothing was rewritten — re-run to refresh",
                describe_branch(self.branch.as_deref()),
                describe_branch(branch.as_deref()),
            ));
        } else if branch.is_none() && into_branch.is_none() {
            blockers.push("HEAD is detached; cannot determine the branch to rewrite".to_string());
        }
        if let Some(name) = into_branch
            && crate::git_io::resolve_commit(repo, &format!("refs/heads/{name}")).is_some()
        {
            blockers.push(format!(
                "branch `{name}` already exists; refusing to overwrite. \
                 Pick a different name or delete it first."
            ));
        }
        Ok(blockers)
    }
}

/// Why the run must not start, and what replaying it came to: the one
/// assessment `--dry` reports and a live run acts on.
fn survey(
    repo: &Path,
    program: &DropProgram,
    into_branch: Option<&str>,
) -> anyhow::Result<(Vec<String>, Option<Assessed>)> {
    let mut blockers = program.blockers(repo, into_branch)?;
    let assessed = if program.linear && !program.leaves_nothing() {
        Some(program.assess(repo)?)
    } else {
        None
    };
    if let (Some(Assessed::Conflict { .. }), Some(name)) = (&assessed, into_branch) {
        blockers.push(format!(
            "a kept commit conflicts, and resolving it needs a git rebase, which \
             cannot write to a new branch `{name}`; run without --into-branch"
        ));
    }
    Ok((blockers, assessed))
}

/// `--dry`: what a live run would drop, keep, and stop at. Writes only
/// unreferenced objects.
pub(crate) fn dry_run(
    repo: &Path,
    program: &DropProgram,
    into_branch: Option<&str>,
) -> anyhow::Result<String> {
    let (blockers, assessed) = survey(repo, program, into_branch)?;
    Ok(render_dry(
        program,
        assessed.as_ref(),
        &blockers,
        into_branch,
    ))
}

/// Run `program`. `Ok(())` once the branch (or `into_branch`) holds the
/// rewritten history, or after `--dry` has printed it; a conflict that
/// stops in a git rebase is an `Err` naming how to finish it.
pub(crate) fn execute(
    repo: &Path,
    program: &DropProgram,
    into_branch: Option<&str>,
    dry: bool,
) -> anyhow::Result<()> {
    if dry {
        print!("{}", dry_run(repo, program, into_branch)?);
        return Ok(());
    }
    let (blockers, assessed) = survey(repo, program, into_branch)?;
    if let Some(first) = blockers.first() {
        anyhow::bail!("purge --drop refuses: {first}");
    }
    match assessed.expect("a program that cannot be assessed always has a blocker") {
        Assessed::Clean { tip, pairs } => land(repo, program, &tip, &pairs, into_branch),
        Assessed::Conflict {
            sha,
            subject,
            paths,
        } => rebase(repo, program, &sha, &subject, &paths),
    }
}

fn land(
    repo: &Path,
    program: &DropProgram,
    tip: &str,
    pairs: &[(CommitSha, CommitSha)],
    into_branch: Option<&str>,
) -> anyhow::Result<()> {
    let branch = match into_branch {
        Some(name) => {
            git_plumbing::update_ref(
                repo,
                &format!("refs/heads/{name}"),
                tip,
                ExpectedRef::CreateOnly,
            )?;
            name.to_string()
        }
        None => {
            let branch = program
                .branch
                .clone()
                .ok_or_else(|| anyhow::anyhow!("HEAD is detached; nothing was rewritten"))?;
            git_plumbing::update_ref(
                repo,
                &format!("refs/heads/{branch}"),
                tip,
                ExpectedRef::Match(program.head_sha.clone()),
            )
            .map_err(|e| {
                anyhow::anyhow!(
                    "branch `{branch}` moved since the plan was read (expected {}); \
                     nothing was rewritten — re-run to refresh. underlying: {e}",
                    short(&program.head_sha),
                )
            })?;
            if git_plumbing::commit_tree_oid(repo, tip)?
                != git_plumbing::commit_tree_oid(repo, &program.head_sha)?
            {
                git_plumbing::reset_hard(repo, "HEAD")?;
            }
            branch
        }
    };
    // A plumbing ref update fires no post-rewrite hook.
    crate::cli::rewire::migrate_feedback_pairs(repo, pairs)?;
    println!(
        "dropped `{}` ({} commit(s)), kept {} other commit(s): branch `{branch}` now at {} ({tip})",
        program.stem,
        program.dropped(),
        program.kept(),
        short(tip),
    );
    Ok(())
}

/// Hand the program to `git rebase --interactive`, from the same base,
/// to stop at the conflict for the operator.
fn rebase(
    repo: &Path,
    program: &DropProgram,
    sha: &str,
    subject: &str,
    paths: &[String],
) -> anyhow::Result<()> {
    // The replay above took time and wrote objects; whatever changed the
    // branch meanwhile must not have a stale todo applied to it.
    if let Some(first) = program.blockers(repo, None)?.first() {
        anyhow::bail!("purge --drop refuses: {first}");
    }
    let branch = program.branch.as_deref().unwrap_or("HEAD");
    eprintln!(
        "{} {subject} does not apply without `{}`'s commits ({}); handing the drop to \
         `git rebase --interactive`.",
        short(sha),
        program.stem,
        paths.join(", "),
    );
    match git_plumbing::rebase_dropping(repo, program.base.as_deref(), &program.todo())? {
        Rebased::Finished => {
            println!(
                "dropped `{}` ({} commit(s)), kept {} other commit(s) via git rebase: \
                 branch `{branch}` now at {}",
                program.stem,
                program.dropped(),
                program.kept(),
                crate::git_io::rev_parse_head(repo)?
                    .map(|s| short(s.as_str()))
                    .unwrap_or_default(),
            );
            Ok(())
        }
        Rebased::Stopped => {
            // The program removes the plan's artifacts from every kept
            // commit anyway; resolving them here leaves the operator only
            // the conflicts that need a human.
            git_plumbing::remove_paths_if_tracked(repo, &program.artifact_paths)?;
            let stopped = crate::git_io::resolve_commit(repo, "REBASE_HEAD")
                .map(|s| s.as_str().to_string())
                .unwrap_or_else(|| sha.to_string());
            let stopped_subject = program
                .steps
                .iter()
                .find(|s| s.sha == stopped)
                .map_or(subject, |s| s.subject.as_str());
            let conflicted = crate::git_io::unmerged_paths_at(repo)?;
            let todo = if conflicted.is_empty() {
                format!(
                    "Its only conflicts were on `{}`'s artifacts, now removed",
                    program.stem
                )
            } else {
                format!(
                    "It conflicts on {} without `{}`'s commits: resolve those paths and \
                     `git add` them",
                    conflicted.join(", "),
                    program.stem
                )
            };
            anyhow::bail!(
                "purge --drop stopped in `git rebase` at {} {stopped_subject}. {todo}; then \
                 `git rebase --continue` finishes the drop (the remaining steps, and the \
                 removal of `{}`'s artifacts from every kept commit, still run). \
                 `git rebase --abort` restores `{branch}` to {} exactly.",
                short(&stopped),
                program.stem,
                short(&program.head_sha),
            )
        }
    }
}

fn render_dry(
    program: &DropProgram,
    assessed: Option<&Assessed>,
    blockers: &[String],
    into_branch: Option<&str>,
) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(out, "# clank purge --drop preview: `{}`", program.stem);
    let _ = writeln!(
        out,
        "# {} commit(s) dropped, {} kept; each kept commit is re-applied as its own change,",
        program.dropped(),
        program.kept(),
    );
    let _ = writeln!(
        out,
        "# then {} removed from it.",
        program.artifact_paths.join(" and ")
    );
    let _ = writeln!(
        out,
        "# base: {}",
        program
            .base
            .as_deref()
            .unwrap_or("(root: rewritten from the empty tree)"),
    );
    let target = match into_branch {
        Some(b) => format!("a NEW branch `{b}` (current branch untouched)"),
        None => format!("{} (in place)", describe_branch(program.branch.as_deref())),
    };
    let _ = writeln!(out, "# target: {target}");
    if let Some(Assessed::Conflict {
        sha,
        subject,
        paths,
    }) = assessed
    {
        let _ = writeln!(
            out,
            "# conflict ahead: {} {subject} conflicts on {} without the plan's commits;",
            short(sha),
            paths.join(", "),
        );
        let _ = writeln!(
            out,
            "# the live run would stop there in `git rebase` for you to resolve."
        );
    }
    if !blockers.is_empty() {
        let _ = writeln!(out, "# BLOCKERS (live run would refuse):");
        for b in blockers {
            let _ = writeln!(out, "#   - {b}");
        }
    }
    let _ = writeln!(out, "#");
    let _ = writeln!(out, "# steps, as a git rebase would run them:");
    for line in program.todo().lines() {
        let _ = writeln!(out, "{line}");
    }
    let _ = writeln!(out, "# (--dry-run: no refs, index or worktree touched)");
    out
}

fn nothing_left_message() -> &'static str {
    "every commit in range would be dropped AND the plan's intro is a root commit — \
     cannot point a branch at 'nothing'. Use `git branch -D` to delete the branch \
     outright if that's what you want."
}

fn describe_branch(branch: Option<&str>) -> String {
    match branch {
        Some(b) => format!("branch `{b}`"),
        None => "a detached HEAD".to_string(),
    }
}

fn parse_sha(sha: &str) -> anyhow::Result<CommitSha> {
    CommitSha::parse(sha).map_err(|e| anyhow::anyhow!("parse sha `{sha}`: {e}"))
}

fn short(sha: &str) -> String {
    sha.chars().take(7).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    use crate::cli::PurgeArgs;
    use crate::cli::monitor::TempRepo;

    fn git(repo: &Path, args: &[&str]) -> String {
        git_env(repo, args, &[])
    }

    fn git_env(repo: &Path, args: &[&str], env: &[(&str, &str)]) -> String {
        let out = std::process::Command::new("git")
            .current_dir(repo)
            .args(args)
            .envs(env.iter().copied())
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    fn write(repo: &Path, rel: &str, body: &str) {
        std::fs::create_dir_all(repo.join(rel).parent().unwrap()).unwrap();
        std::fs::write(repo.join(rel), body).unwrap();
    }

    fn init(with_base: bool) -> TempRepo {
        let dir = TempRepo::new();
        let p = dir.path();
        git(p, &["init", "--quiet", "--initial-branch=main"]);
        git(p, &["config", "user.email", "runner@test"]);
        git(p, &["config", "user.name", "runner"]);
        git(p, &["config", "commit.gpgsign", "false"]);
        write(p, ".clank/.gitignore", "/cache/\n/agents/\n/stash/\n");
        if with_base {
            write(
                p,
                ".clank/config.json",
                r#"{"agents":{"claude":{"tool":"claude","role":"master"}}}"#,
            );
            write(p, "base.rs", "// base\n");
            commit(p, "base", "Base Author", 1_000);
        }
        dir
    }

    /// Commit everything staged-or-not as `author`, authored at `ts`, so
    /// the rewrite's metadata can be told apart from the runner's.
    fn commit(repo: &Path, msg: &str, author: &str, ts: u64) -> String {
        let email = format!("{}@test", author.replace(' ', "."));
        let date = format!("{} +0000", 1_700_000_000 + ts);
        git(repo, &["add", "-A"]);
        git_env(
            repo,
            &["commit", "--quiet", "--allow-empty", "-m", msg],
            &[
                ("GIT_AUTHOR_NAME", author),
                ("GIT_AUTHOR_EMAIL", &email),
                ("GIT_AUTHOR_DATE", &date),
            ],
        );
        git(repo, &["rev-parse", "HEAD"])
    }

    fn finish(repo: &Path, stem: &str, ts: u64) -> String {
        let plan = format!(".clank/plans/{stem}.md");
        let body = std::fs::read_to_string(repo.join(&plan)).unwrap();
        git(repo, &["rm", "--quiet", &plan]);
        write(repo, &format!(".clank/finished/{stem}.md"), &body);
        commit(repo, &format!("[{stem}] finish"), "Plan Author", ts)
    }

    /// `(subject, author name, author email, author date)` oldest first.
    fn history(repo: &Path, rev: &str) -> Vec<(String, String, String, String)> {
        git(
            repo,
            &["log", "--reverse", "--format=%s%x1f%an%x1f%ae%x1f%ad", rev],
        )
        .lines()
        .map(|l| {
            let f: Vec<&str> = l.split('\x1f').collect();
            (f[0].into(), f[1].into(), f[2].into(), f[3].into())
        })
        .collect()
    }

    fn files(repo: &Path, rev: &str) -> BTreeMap<String, String> {
        git(repo, &["ls-tree", "-r", "--name-only", rev])
            .lines()
            .map(|path| {
                (
                    path.to_string(),
                    git(repo, &["show", &format!("{rev}:{path}")]),
                )
            })
            .collect()
    }

    fn args(plan: &str, into_branch: Option<&str>, dry: bool, repo: &Path) -> PurgeArgs {
        PurgeArgs {
            plan: Some(plan.into()),
            all: false,
            repo: Some(repo.to_path_buf()),
            into_branch: into_branch.map(String::from),
            dry,
            squash: None,
            amend: false,
            no_cache: true,
            drop: true,
        }
    }

    async fn drop_plan(repo: &Path, plan: &str) -> anyhow::Result<()> {
        crate::cli::purge::run(args(plan, None, false, repo)).await
    }

    async fn program(repo: &Path, plan: &str) -> DropProgram {
        let state =
            crate::rebuild::rebuild_repo_with_policy(repo, crate::rebuild::CachePolicy::Bypass)
                .await
                .unwrap();
        let key = crate::lifecycle::PlanKey::parse(plan).unwrap();
        let preview = crate::preview::build_rewrite_preview(repo, &state, &key, true)
            .await
            .unwrap();
        DropProgram::from_preview(repo, &preview).unwrap()
    }

    /// Everything a refused or dry run must leave alone.
    fn snapshot(repo: &Path) -> (String, String, Option<String>, Vec<String>) {
        (
            git(repo, &["for-each-ref", "--format=%(refname) %(objectname)"]),
            git(repo, &["rev-parse", "HEAD"]),
            crate::git_io::index_digest_at(repo).unwrap(),
            crate::git_io::working_tree_dirty_paths(repo).unwrap(),
        )
    }

    fn rebase_in_progress(repo: &Path) -> bool {
        crate::git_io::operation_in_progress_at(repo)
            .unwrap()
            .is_some()
    }

    /// Plan `a` finished a while ago, followed by another plan's commits
    /// and the user's own.
    fn buried_plan() -> TempRepo {
        let dir = init(true);
        let p = dir.path();
        write(p, ".clank/plans/a.md", "# a\n");
        write(p, "a.rs", "fn a() {}\n");
        commit(p, "[a] intro", "Plan Author", 2_000);
        write(p, "a.rs", "fn a() { 1 }\n");
        write(p, "shared.rs", "// a was here\n");
        commit(p, "[a] impl", "Plan Author", 3_000);
        finish(p, "a", 4_000);
        write(p, ".clank/plans/b.md", "# b\n");
        write(p, "b.rs", "fn b() {}\n");
        commit(p, "[b] intro", "B Author", 5_000);
        write(p, "b.rs", "fn b() { 2 }\n");
        commit(p, "[b] work", "B Author", 6_000);
        write(p, "user.txt", "mine\n");
        commit(p, "user tweak", "User", 7_000);
        dir
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_buried_plan_goes_and_everything_after_it_stays() {
        let dir = buried_plan();
        let p = dir.path();
        let before = history(p, "HEAD");
        let mut want = files(p, "HEAD");
        for gone in ["a.rs", "shared.rs", ".clank/finished/a.md"] {
            assert!(want.remove(gone).is_some(), "fixture has {gone}");
        }

        drop_plan(p, "a").await.unwrap();

        let after = history(p, "HEAD");
        let kept: Vec<_> = before
            .iter()
            .filter(|c| !c.0.starts_with("[a]"))
            .cloned()
            .collect();
        assert_eq!(
            after, kept,
            "only a's commits went, the rest keep message, author and date"
        );
        assert_eq!(
            files(p, "HEAD"),
            want,
            "the tree is the old one minus a's changes"
        );
        for rev in ["HEAD~2", "HEAD~1", "HEAD"] {
            let tree = files(p, rev);
            assert!(
                !tree.contains_key(".clank/plans/a.md")
                    && !tree.contains_key(".clank/finished/a.md"),
                "no plan file reappears at {rev}: {tree:?}"
            );
        }
        assert_eq!(
            git(p, &["log", "-1", "--format=%cn", "HEAD"]),
            "runner",
            "the committer is whoever ran it"
        );
        assert!(
            crate::git_io::working_tree_dirty_paths(p)
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn interleaved_commits_keep_their_order() {
        let dir = init(true);
        let p = dir.path();
        write(p, ".clank/plans/a.md", "# a\n");
        write(p, "a.rs", "1\n");
        commit(p, "[a] intro", "Plan Author", 2_000);
        write(p, "b.rs", "1\n");
        commit(p, "[b] first", "B Author", 3_000);
        write(p, "a2.rs", "2\n");
        commit(p, "[a] more", "Plan Author", 4_000);
        write(p, "b.rs", "1\n2\n");
        commit(p, "[b] second", "B Author", 5_000);

        drop_plan(p, "a").await.unwrap();

        let subjects: Vec<String> = history(p, "HEAD").into_iter().map(|c| c.0).collect();
        assert_eq!(subjects, ["base", "[b] first", "[b] second"]);
        let tree = files(p, "HEAD");
        assert_eq!(tree.get("b.rs").map(String::as_str), Some("1\n2"));
        for gone in ["a.rs", "a2.rs", ".clank/plans/a.md"] {
            assert!(!tree.contains_key(gone), "{gone} went: {tree:?}");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_empty_or_artifact_only_kept_commit_stays_a_commit() {
        let dir = init(true);
        let p = dir.path();
        write(p, ".clank/plans/a.md", "# a\n");
        write(p, "a.rs", "1\n");
        commit(p, "[a] intro", "Plan Author", 2_000);
        commit(p, "empty note", "User", 3_000);
        write(p, ".clank/plans/b.md", "# b\n");
        commit(p, "queue b", "User", 4_000);
        write(p, "a.rs", "1\n2\n");
        commit(p, "[a] impl", "Plan Author", 5_000);

        let program = program(p, "a").await;
        assert_eq!(
            program.steps.iter().map(|s| s.keep).collect::<Vec<_>>(),
            [false, true, true, false],
            "the fixture: two kept commits the old engine would have dropped"
        );
        drop_plan(p, "a").await.unwrap();

        let subjects: Vec<String> = history(p, "HEAD").into_iter().map(|c| c.0).collect();
        assert_eq!(subjects, ["base", "empty note", "queue b"]);
        assert_eq!(
            git(p, &["rev-parse", "HEAD~1^{tree}"]),
            git(p, &["rev-parse", "HEAD~2^{tree}"]),
            "the empty commit is still there, still empty"
        );
        assert!(files(p, "HEAD").contains_key(".clank/plans/b.md"));
        assert!(!files(p, "HEAD").contains_key(".clank/plans/a.md"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_plan_at_the_root_drops_onto_the_empty_tree() {
        let dir = init(false);
        let p = dir.path();
        write(p, ".clank/plans/a.md", "# a\n");
        write(p, "a.rs", "1\n");
        commit(p, "[a] intro", "Plan Author", 2_000);
        write(p, "user.txt", "mine\n");
        commit(p, "user work", "User", 3_000);

        drop_plan(p, "a").await.unwrap();

        assert_eq!(git(p, &["rev-list", "--count", "HEAD"]), "1");
        assert_eq!(
            git(p, &["log", "-1", "--format=%P%x1f%s%x1f%an"]),
            "\x1fuser work\x1fUser"
        );
        assert_eq!(
            files(p, "HEAD").into_keys().collect::<Vec<_>>(),
            ["user.txt"]
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropping_a_whole_history_from_its_root_is_refused() {
        let dir = init(false);
        let p = dir.path();
        write(p, ".clank/plans/a.md", "# a\n");
        commit(p, "[a] intro", "Plan Author", 2_000);
        let before = snapshot(p);

        let err = drop_plan(p, "a").await.unwrap_err();

        assert!(
            format!("{err:#}").contains("cannot point a branch at 'nothing'"),
            "{err:#}"
        );
        assert_eq!(snapshot(p), before);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_plan_at_the_tip_leaves_the_branch_at_its_parent() {
        let dir = init(true);
        let p = dir.path();
        let base = git(p, &["rev-parse", "HEAD"]);
        write(p, ".clank/plans/a.md", "# a\n");
        write(p, "a.rs", "1\n");
        commit(p, "[a] intro", "Plan Author", 2_000);
        write(p, "a.rs", "2\n");
        commit(p, "[a] impl", "Plan Author", 3_000);

        drop_plan(p, "a").await.unwrap();

        assert_eq!(git(p, &["rev-parse", "HEAD"]), base);
        assert_eq!(git(p, &["symbolic-ref", "HEAD"]), "refs/heads/main");
        assert!(!p.join("a.rs").exists() && !p.join(".clank/plans/a.md").exists());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn into_branch_writes_the_new_chain_and_leaves_the_current_one() {
        let dir = buried_plan();
        let p = dir.path();
        let head = git(p, &["rev-parse", "HEAD"]);

        crate::cli::purge::run(args("a", Some("scrubbed"), false, p))
            .await
            .unwrap();

        assert_eq!(git(p, &["rev-parse", "HEAD"]), head);
        assert_eq!(git(p, &["symbolic-ref", "HEAD"]), "refs/heads/main");
        let subjects: Vec<String> = history(p, "scrubbed").into_iter().map(|c| c.0).collect();
        assert_eq!(subjects, ["base", "[b] intro", "[b] work", "user tweak"]);
        assert!(!files(p, "scrubbed").contains_key("a.rs"));
    }

    /// Plan `a` introduces `a.rs`; a later commit that is not a's edits
    /// it, and annotates a's finished plan, so it cannot apply without
    /// a's commits. `[b] intro` before it applies cleanly, and so does
    /// `after`, which puts a's plan file back.
    fn conflicting(with_base: bool) -> TempRepo {
        let dir = init(with_base);
        let p = dir.path();
        write(p, ".clank/plans/a.md", "# a\n");
        write(p, "a.rs", "one\n");
        commit(p, "[a] intro", "Plan Author", 2_000);
        finish(p, "a", 3_000);
        write(p, ".clank/plans/b.md", "# b\n");
        commit(p, "[b] intro", "B Author", 4_000);
        write(p, "a.rs", "one\ntwo\n");
        write(p, ".clank/finished/a.md", "# a\n\nnote\n");
        commit(p, "edit a", "User", 5_000);
        write(p, "after.txt", "later\n");
        write(p, ".clank/plans/a.md", "# a, restored by hand\n");
        commit(p, "after", "User", 6_000);
        dir
    }

    fn sha_of(repo: &Path, subject: &str) -> String {
        git(
            repo,
            &["log", "--format=%H", &format!("--grep=^{subject}$"), "HEAD"],
        )
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_conflict_stops_in_a_rebase_that_continues_to_the_final_tree() {
        let dir = conflicting(true);
        let p = dir.path();
        let before = history(p, "HEAD");
        let edit = sha_of(p, "edit a");
        let after = sha_of(p, "after");
        let reviews = p.join(".clank/agents/codex/feedback");
        std::fs::create_dir_all(&reviews).unwrap();
        for (sha, verdict) in [(&edit, "CONTINUE edit\n"), (&after, "CONTINUE after\n")] {
            std::fs::write(reviews.join(format!("{sha}.md")), verdict).unwrap();
        }
        let hook = p.join(".git/hooks/post-rewrite");
        write(
            p,
            ".git/hooks/post-rewrite",
            "#!/bin/sh\ncat >> \"$(git rev-parse --git-dir)/test-rewritten\"\n",
        );
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();

        let err = format!("{:#}", drop_plan(p, "a").await.unwrap_err());

        assert!(rebase_in_progress(p), "stopped in git's own rebase: {err}");
        assert!(
            err.contains(&edit[..7]) && err.contains("edit a"),
            "names the commit: {err}"
        );
        assert!(
            err.contains("conflicts on a.rs without"),
            "names the paths, the artifact already resolved: {err}"
        );
        assert!(
            err.contains("git rebase --continue") && err.contains("git rebase --abort"),
            "says how to finish or back out: {err}"
        );

        git(p, &["add", "a.rs"]);
        git_env(p, &["rebase", "--continue"], &[("GIT_EDITOR", "true")]);
        assert!(!rebase_in_progress(p));

        let kept: Vec<_> = before
            .iter()
            .filter(|c| !c.0.starts_with("[a]"))
            .cloned()
            .collect();
        assert_eq!(
            history(p, "HEAD"),
            kept,
            "later commits keep metadata and order"
        );
        let tree = files(p, "HEAD");
        assert_eq!(tree.get("a.rs").map(String::as_str), Some("one\ntwo"));
        assert!(tree.contains_key("after.txt") && tree.contains_key(".clank/plans/b.md"));
        for rev in ["HEAD~1", "HEAD"] {
            let tree = files(p, rev);
            assert!(
                !tree.contains_key(".clank/finished/a.md")
                    && !tree.contains_key(".clank/plans/a.md"),
                "a's artifacts stay gone at {rev}: {tree:?}"
            );
        }

        let rewritten =
            std::fs::read_to_string(p.join(".git/test-rewritten")).expect("the hook ran");
        crate::cli::rewire::rewire_hook_input(p, &rewritten).unwrap();
        for (rev, verdict) in [("HEAD~1", "CONTINUE edit\n"), ("HEAD", "CONTINUE after\n")] {
            let new = git(p, &["rev-parse", rev]);
            let found = [new.clone(), new[..7].to_string()]
                .iter()
                .map(|name| reviews.join(format!("{name}.md")))
                .find(|f| f.is_file())
                .unwrap_or_else(|| panic!("feedback moved to {rev} ({new})"));
            assert_eq!(std::fs::read_to_string(found).unwrap(), verdict);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn aborting_the_stopped_rebase_restores_the_branch_exactly() {
        let dir = conflicting(true);
        let p = dir.path();
        let before = snapshot(p);

        drop_plan(p, "a").await.unwrap_err();
        assert!(rebase_in_progress(p));
        git(p, &["rebase", "--abort"]);

        assert_eq!(snapshot(p), before);
        assert_eq!(git(p, &["symbolic-ref", "HEAD"]), "refs/heads/main");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_conflict_at_a_root_plan_rebases_from_the_root() {
        let dir = init(false);
        let p = dir.path();
        write(p, ".clank/plans/a.md", "# a\n");
        write(p, "a.rs", "one\n");
        commit(p, "[a] intro", "Plan Author", 2_000);
        write(p, "user.txt", "mine\n");
        commit(p, "user work", "User", 3_000);
        write(p, "a.rs", "one\ntwo\n");
        commit(p, "edit a", "User", 4_000);

        let err = format!("{:#}", drop_plan(p, "a").await.unwrap_err());
        assert!(rebase_in_progress(p), "{err}");
        git(p, &["add", "-A"]);
        git_env(p, &["rebase", "--continue"], &[("GIT_EDITOR", "true")]);

        assert_eq!(
            git(p, &["log", "--reverse", "--format=%P%x1f%s%x1f%an"]),
            "\x1fuser work\x1fUser\n".to_string()
                + &git(p, &["rev-parse", "HEAD~1"])
                + "\x1fedit a\x1fUser",
            "the first kept commit is the new root"
        );
        assert_eq!(
            files(p, "HEAD").into_keys().collect::<Vec<_>>(),
            ["a.rs", "user.txt"]
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dry_reports_the_conflict_ahead_and_touches_nothing() {
        let dir = conflicting(true);
        let p = dir.path();
        let before = snapshot(p);
        let edit = sha_of(p, "edit a");

        let report = dry_run(p, &program(p, "a").await, None).unwrap();
        crate::cli::purge::run(args("a", None, true, p))
            .await
            .unwrap();

        assert!(
            report.contains("conflict ahead") && report.contains(&edit[..7]),
            "{report}"
        );
        assert!(!report.contains("BLOCKERS"), "{report}");
        assert_eq!(snapshot(p), before);
        assert!(!rebase_in_progress(p));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_dirty_tree_is_refused() {
        let dir = buried_plan();
        let p = dir.path();
        write(p, "user.txt", "unsaved\n");
        let before = snapshot(p);

        let err = format!("{:#}", drop_plan(p, "a").await.unwrap_err());

        assert!(err.contains("working tree dirty"), "{err}");
        assert_eq!(snapshot(p), before);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_rebase_already_in_progress_is_refused() {
        let dir = buried_plan();
        let p = dir.path();
        git_env(
            p,
            &["rebase", "--interactive", "HEAD~1"],
            &[("GIT_SEQUENCE_EDITOR", "sed -i.bak 's/^pick/edit/'")],
        );
        assert!(rebase_in_progress(p), "the fixture stopped at an edit");
        let before = snapshot(p);

        let err = format!("{:#}", drop_plan(p, "a").await.unwrap_err());

        assert!(err.contains("`git rebase` is in progress"), "{err}");
        assert_eq!(snapshot(p), before);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn head_moved_since_the_preview_is_refused() {
        let dir = conflicting(true);
        let p = dir.path();
        let program = program(p, "a").await;
        write(p, "late.txt", "late\n");
        commit(p, "late", "User", 9_000);
        let before = snapshot(p);

        let err = format!("{:#}", execute(p, &program, None, false).unwrap_err());

        assert!(err.contains("HEAD moved"), "{err}");
        assert_eq!(snapshot(p), before);
        assert!(!rebase_in_progress(p));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn into_branch_with_a_conflict_is_refused() {
        let dir = conflicting(true);
        let p = dir.path();
        let before = snapshot(p);

        let err = format!(
            "{:#}",
            crate::cli::purge::run(args("a", Some("scrubbed"), false, p))
                .await
                .unwrap_err()
        );

        assert!(err.contains("run without --into-branch"), "{err}");
        assert_eq!(snapshot(p), before);
        assert!(!rebase_in_progress(p));
    }

    /// git reads `git rm`'s arguments as patterns, and a plan name may
    /// hold `*`: the cleanup must still remove only the plan's own files.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_glob_named_plan_cannot_delete_another_plans_artifacts() {
        let dir = init(true);
        let p = dir.path();
        assert!(crate::lifecycle::PlanKey::parse("a*").is_ok());
        write(p, ".clank/plans/a*.md", "# a*\n");
        write(p, "a.rs", "one\n");
        commit(p, "[a*] intro", "Plan Author", 2_000);
        write(p, ".clank/plans/another.md", "# another\n");
        commit(p, "[another] intro", "Other Author", 3_000);
        write(p, "a.rs", "one\ntwo\n");
        commit(p, "edit a", "User", 4_000);

        let err = drop_plan(p, "a*").await.unwrap_err();
        assert!(rebase_in_progress(p), "{err:#}");
        assert!(
            p.join(".clank/plans/another.md").is_file(),
            "the stop's own cleanup kept it"
        );
        git(p, &["add", "a.rs"]);
        git_env(p, &["rebase", "--continue"], &[("GIT_EDITOR", "true")]);

        assert!(!rebase_in_progress(p));
        for rev in ["HEAD~1", "HEAD"] {
            let tree = files(p, rev);
            assert!(
                tree.contains_key(".clank/plans/another.md"),
                "the other plan survives at {rev}: {tree:?}"
            );
            assert!(!tree.contains_key(".clank/plans/a*.md"), "{tree:?}");
        }
    }

    /// The first kept commit of a root drop is the new root even when it
    /// is the one that conflicts, and git's `--root` placeholder commit
    /// never reaches the history.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_first_kept_commit_that_conflicts_becomes_the_root() {
        let dir = init(false);
        let p = dir.path();
        write(p, ".clank/plans/a.md", "# a\n");
        write(p, "a.rs", "one\n");
        commit(p, "[a] intro", "Plan Author", 2_000);
        write(p, "a.rs", "one\ntwo\n");
        let edit = commit(p, "edit a", "User", 4_000);
        let before = history(p, "HEAD");
        let reviews = p.join(".clank/agents/codex/feedback");
        std::fs::create_dir_all(&reviews).unwrap();
        std::fs::write(reviews.join(format!("{edit}.md")), "CONTINUE edit\n").unwrap();
        write(
            p,
            ".git/hooks/post-rewrite",
            "#!/bin/sh\ncat >> \"$(git rev-parse --git-dir)/test-rewritten\"\n",
        );
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(
            p.join(".git/hooks/post-rewrite"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();

        drop_plan(p, "a").await.unwrap_err();
        assert!(rebase_in_progress(p));
        git(p, &["add", "a.rs"]);
        git_env(p, &["rebase", "--continue"], &[("GIT_EDITOR", "true")]);

        assert!(!rebase_in_progress(p));
        assert_eq!(history(p, "HEAD"), before[1..].to_vec());
        assert_eq!(git(p, &["log", "-1", "--format=%P"]), "", "a root commit");
        assert_eq!(files(p, "HEAD").into_keys().collect::<Vec<_>>(), ["a.rs"]);

        let rewritten =
            std::fs::read_to_string(p.join(".git/test-rewritten")).expect("the hook ran");
        crate::cli::rewire::rewire_hook_input(p, &rewritten).unwrap();
        let new = git(p, &["rev-parse", "HEAD"]);
        assert!(
            [new.clone(), new[..7].to_string()]
                .iter()
                .any(|name| reviews.join(format!("{name}.md")).is_file()),
            "the review moved to the new root"
        );
    }

    #[test]
    fn the_cleanup_quotes_every_path_literally() {
        let todo = git_plumbing::drop_rebase_todo(
            &[RebaseStep::Pick {
                sha: "abc",
                subject: "s",
            }],
            &[".clank/plans/it's $(x).md".to_string()],
            false,
        );
        assert!(
            todo.contains(r"-- '.clank/plans/it'\''s $(x).md' &&"),
            "{todo}"
        );
    }
}
