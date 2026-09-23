//! `clank feedback write` and `clank feedback read`.

use std::io::Write;
use std::path::Path;

use anyhow::Context;

use super::{FeedbackCmd, FeedbackReadArgs, FeedbackWriteArgs, resolve_repo};
use crate::disk_format::feedback_path_wire;
use crate::lifecycle::{AgentLabel, CommitRef, CommitSha, PlanKey};
use clank_core::ids::CommitRefResolveError;

pub async fn run(args: super::FeedbackArgs) -> anyhow::Result<()> {
    match args.command {
        FeedbackCmd::Write(write) => run_write(write).await,
        FeedbackCmd::Read(read) => {
            print!("{}", read_rendered(read).await?);
            Ok(())
        }
    }
}

async fn run_write(args: FeedbackWriteArgs) -> anyhow::Result<()> {
    let repo = resolve_repo(args.repo.as_deref())?;

    let commit_ref = CommitRef::parse(&args.commit)
        .map_err(|e| anyhow::anyhow!("invalid --commit `{}`: {e}", args.commit))?;
    let author = AgentLabel::parse(&args.author)
        .map_err(|e| anyhow::anyhow!("invalid --author `{}`: {e}", args.author))?;
    let expected_verdict: clank_core::Verdict = args.verdict.into();
    let verdict_header = match expected_verdict {
        clank_core::Verdict::Continue => "CONTINUE",
        clank_core::Verdict::Finished => "FINISHED",
        clank_core::Verdict::RequestChanges => "REQUEST_CHANGES",
        clank_core::Verdict::Unmarked => {
            anyhow::bail!("verdict `unmarked` cannot be written");
        }
    };

    // The declared --verdict is the single source of truth: a leading
    // restatement in the message ("CONTINUE: …") would render the
    // verdict twice on every summary surface, so it's normalized away
    // here at the boundary (strip-verdict-restatement).
    let message =
        clank_core::feedback_body::strip_verdict_restatement(expected_verdict, args.message.trim());
    // No space after the header when the summary is empty or the
    // stripped first line was empty (the remainder starts with the
    // body's newlines).
    let sep = if message.is_empty() || message.starts_with('\n') {
        ""
    } else {
        " "
    };
    let body = format!("{verdict_header}{sep}{message}\n");

    let state =
        crate::rebuild::rebuild_repo_with_policy(&repo, crate::rebuild::CachePolicy::Bypass)
            .await
            .map_err(|e| anyhow::anyhow!("failed to fold repo `{}`: {e}", repo.display()))?;

    let mut all_shas: Vec<CommitSha> = Vec::new();
    for ps in state.fold.plans.values() {
        all_shas.extend(ps.reviewable_shas());
    }
    for ah in &state.fold.ad_hoc {
        all_shas.push(ah.sha.clone());
    }
    all_shas.sort();
    all_shas.dedup();

    let target_sha = commit_ref.resolve_against(&all_shas).map_err(|e| match e {
        CommitRefResolveError::Orphan => anyhow::anyhow!(
            "--commit `{}` did not match any known commit. \
                 Known shas: {}",
            args.commit,
            format_short_list(&all_shas),
        ),
        CommitRefResolveError::Ambiguous { matches } => anyhow::anyhow!(
            "--commit `{}` is ambiguous (matched {} commits): \
                 {}. Pass a longer prefix or the full SHA.",
            args.commit,
            matches.len(),
            format_short_list(&matches),
        ),
    })?;

    let wire_path = feedback_path_wire(&author, &target_sha, &all_shas);
    let abs_path = repo.join(&wire_path);

    if let Some(parent) = abs_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating `{}`", parent.display()))?;
    }

    write_atomic(&abs_path, body.as_bytes())
        .with_context(|| format!("writing `{}`", abs_path.display()))?;

    println!("{wire_path}");
    Ok(())
}

fn write_atomic(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut tmp = tempfile::Builder::new()
        .prefix(".clank-feedback-")
        .suffix(".md.tmp")
        .tempfile_in(parent)?;
    tmp.write_all(contents)?;
    tmp.as_file_mut().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

fn format_short_list(shas: &[CommitSha]) -> String {
    if shas.is_empty() {
        return "(none — plan has no reviewable commits yet)".into();
    }
    shas.iter()
        .map(|s| s.as_str()[..7].to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// What `clank feedback read` prints. `run` is the only printer, so
/// both readings can be asserted in-process.
pub async fn read_rendered(args: FeedbackReadArgs) -> anyhow::Result<String> {
    let repo = resolve_repo(args.repo.as_deref())?;
    match args.plan.as_deref() {
        Some(plan_arg) => read_plan(&repo, plan_arg, args.json).await,
        None => read_commit(&repo, args.commit.as_deref(), args.json),
    }
}

fn read_commit(repo: &Path, rev: Option<&str>, json: bool) -> anyhow::Result<String> {
    let rev = rev.unwrap_or("HEAD");
    let full_sha = crate::git_io::resolve_commit(repo, rev)
        .ok_or_else(|| anyhow::anyhow!("cannot resolve `{rev}` to a commit"))?
        .as_str()
        .to_string();
    let entries = entries_for_commit(repo, &full_sha)?;

    if entries.is_empty() {
        // Prose even under `-j`: this reading is older than `--plan`
        // and is promised unchanged. The plan reading below is JSON
        // whenever `-j` is passed.
        return Ok(format!("no feedback for {}\n", short(&full_sha)));
    }
    if json {
        Ok(format!("{}\n", serde_json::to_string_pretty(&entries)?))
    } else {
        Ok(commit_block(short(&full_sha), &entries))
    }
}

async fn read_plan(repo: &Path, plan_arg: &str, json: bool) -> anyhow::Result<String> {
    let reading = plan_reading(repo, plan_arg).await?;
    if json {
        Ok(format!("{}\n", serde_json::to_string_pretty(&reading)?))
    } else {
        Ok(reading.text())
    }
}

async fn plan_reading(repo: &Path, plan_arg: &str) -> anyhow::Result<PlanReading> {
    let basename = super::repo_basename(repo)?;
    let state = crate::rebuild::rebuild_repo_with_policy(repo, crate::rebuild::CachePolicy::Use)
        .await
        .map_err(|e| anyhow::anyhow!("failed to fold repo `{}`: {e}", repo.display()))?;
    // Bare `--plan` arrives as the empty string: infer the active plan.
    let named = Some(plan_arg).filter(|s| !s.trim().is_empty());
    let key = crate::cli::plan_resolve::resolve_plan(&state, &basename, named)?;
    let history = crate::cli::plan_resolve::plan_history(repo, &state, &key).await?;

    let mut groups: Vec<CommitFeedback> = Vec::new();
    for sha in history.commits {
        let feedback = entries_for_commit(repo, sha.as_str())?;
        groups.push(CommitFeedback {
            commit: sha,
            feedback,
        });
    }

    Ok(PlanReading {
        // A plan whose intro IS its finalize has had its commits
        // collapsed into one — the shape a squashing finish leaves,
        // and the only collapse this can observe. A `--no-squash`
        // finish always adds a finalize commit, so the two differ.
        collapsed: history
            .finished
            .as_ref()
            .is_some_and(|f| f.intro == f.finalized_at),
        finalized_at: history.finished.map(|f| f.finalized_at),
        plan: key,
        commits: groups,
    })
}

/// One plan's review cycle: the commits it is attributed, each with
/// whatever was written against that sha. Rendered as text, or
/// serialized whole for `-j`.
#[derive(serde::Serialize)]
struct PlanReading {
    plan: PlanKey,
    /// The commit `finish` left behind, if the plan is finished.
    finalized_at: Option<CommitSha>,
    /// Whether that finish collapsed the plan's commits into one.
    collapsed: bool,
    commits: Vec<CommitFeedback>,
}

#[derive(serde::Serialize)]
struct CommitFeedback {
    commit: CommitSha,
    feedback: Vec<FeedbackEntry>,
}

impl PlanReading {
    fn text(&self) -> String {
        let plan = self.plan.as_str();
        if self.commits.is_empty() {
            return format!("plan {plan} has no commits yet\n");
        }
        if self.commits.iter().all(|c| c.feedback.is_empty()) {
            let n = self.commits.len();
            let s = if n == 1 { "" } else { "s" };
            return match (&self.finalized_at, self.collapsed) {
                // Saying "none reviewed yet" of a plan whose cycle
                // was collapsed is the false reading this command
                // exists to remove: the commit that survived is one
                // nobody ever reviewed.
                (Some(sha), true) => format!(
                    "plan {plan} was finished at {}, which is also its intro.\n\
                     The cycle was collapsed into that one commit, which nobody reviewed. Any\n\
                     verdicts written during it stay keyed by the shas that were reviewed, and\n\
                     those are no longer in history. `clank feedback read --commit <sha>` reads\n\
                     any sha that still resolves.\n",
                    short(sha.as_str()),
                ),
                (Some(sha), false) => format!(
                    "no feedback for plan {plan} (finished at {}, {n} commit{s}, none of them \
                     reviewed)\n",
                    short(sha.as_str()),
                ),
                (None, _) => {
                    format!("no feedback for plan {plan} ({n} commit{s}, none reviewed yet)\n")
                }
            };
        }

        let mut out = format!("plan {plan}\n");
        for c in &self.commits {
            let sha = short(c.commit.as_str());
            if c.feedback.is_empty() {
                out.push_str(&format!("commit {sha}\n  no feedback\n"));
            } else {
                out.push_str(&commit_block(sha, &c.feedback));
            }
        }
        out
    }
}

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(7)]
}

/// Every reviewer's feedback for one commit, by author.
fn entries_for_commit(repo: &Path, full_sha: &str) -> anyhow::Result<Vec<FeedbackEntry>> {
    let short_sha = short(full_sha);
    let agents_dir = repo.join(".clank/agents");
    let mut entries: Vec<FeedbackEntry> = Vec::new();

    if let Ok(agents) = std::fs::read_dir(&agents_dir) {
        for agent_entry in agents.flatten() {
            let agent_name = agent_entry.file_name();
            let Some(label) = agent_name.to_str() else {
                continue;
            };
            let feedback_dir = agent_entry.path().join("feedback");
            let candidates = [
                feedback_dir.join(format!("{full_sha}.md")),
                feedback_dir.join(format!("{short_sha}.md")),
            ];
            for path in &candidates {
                if path.exists() {
                    let body = std::fs::read_to_string(path)?;
                    let fb = clank_core::feedback_body::FeedbackBody::parse(&body);
                    let rel = path
                        .strip_prefix(repo)
                        .map(|p| p.to_string_lossy().to_string())
                        .unwrap_or_else(|_| path.to_string_lossy().to_string());
                    entries.push(FeedbackEntry {
                        author: label.to_string(),
                        verdict: fb.verdict,
                        summary: fb.summary(),
                        details: fb.details(),
                        source_path: rel,
                    });
                    break;
                }
            }
        }
    }

    entries.sort_by(|a, b| a.author.cmp(&b.author));
    Ok(entries)
}

fn commit_block(short_sha: &str, entries: &[FeedbackEntry]) -> String {
    let mut out = format!("commit {short_sha}\n");
    for e in entries {
        let verdict_str = match e.verdict {
            clank_core::Verdict::Continue => "CONTINUE",
            clank_core::Verdict::Finished => "FINISHED",
            clank_core::Verdict::RequestChanges => "REQUEST_CHANGES",
            clank_core::Verdict::Unmarked => "UNMARKED",
        };
        let summary = if e.summary.is_empty() {
            String::new()
        } else {
            format!(": {}", e.summary)
        };
        out.push_str(&format!("  {}  {}{}\n", e.author, verdict_str, summary));
        if !e.details.is_empty() {
            for line in e.details.lines() {
                out.push_str(&format!("    {line}\n"));
            }
        }
        out.push_str(&format!("    {}\n", e.source_path));
    }
    out
}

/// One reviewer's feedback on one commit. `verdict` serializes as
/// the lowercase wire string (not the typed enum's own Serialize),
/// which is the row shape `-j` has always emitted.
#[derive(serde::Serialize)]
struct FeedbackEntry {
    author: String,
    #[serde(serialize_with = "verdict_wire")]
    verdict: clank_core::Verdict,
    summary: String,
    details: String,
    source_path: String,
}

fn verdict_wire<S: serde::Serializer>(
    verdict: &clank_core::Verdict,
    s: S,
) -> Result<S::Ok, S::Error> {
    s.serialize_str(verdict.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feedback_entry_json_matches_prior_shape() {
        // typed-json-not-json-macro: the row serializes to the same
        // keys+values the old `json!` produced. `verdict` is the
        // lowercase wire string, not the typed enum.
        let entry = FeedbackEntry {
            author: "codex".to_string(),
            verdict: clank_core::Verdict::RequestChanges,
            summary: "needs work".to_string(),
            details: "line one\nline two".to_string(),
            source_path: ".clank/agents/codex/feedback/abc.md".to_string(),
        };
        assert_eq!(
            serde_json::to_value(&entry).unwrap(),
            serde_json::json!({
                "author": "codex",
                "verdict": clank_core::Verdict::RequestChanges.as_str(),
                "summary": "needs work",
                "details": "line one\nline two",
                "source_path": ".clank/agents/codex/feedback/abc.md",
            })
        );
    }
}
