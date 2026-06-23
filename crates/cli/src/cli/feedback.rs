//! `clank feedback write` and `clank feedback read`.

use std::io::Write;
use std::path::Path;

use anyhow::Context;

use super::{FeedbackCmd, FeedbackReadArgs, FeedbackWriteArgs, resolve_repo};
use crate::disk_format::feedback_path_wire;
use crate::lifecycle::{AgentLabel, CommitRef, CommitSha};
use clank_core::ids::CommitRefResolveError;

pub async fn run(args: super::FeedbackArgs) -> anyhow::Result<()> {
    match args.command {
        FeedbackCmd::Write(write) => run_write(write).await,
        FeedbackCmd::Read(read) => run_read(read).await,
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

    let body = format!("{verdict_header} {}\n", args.message.trim());

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

async fn run_read(args: FeedbackReadArgs) -> anyhow::Result<()> {
    let repo = resolve_repo(args.repo.as_deref())?;

    let rev = args.commit.as_deref().unwrap_or("HEAD");
    let full_sha = crate::git_io::resolve_commit(&repo, rev)
        .ok_or_else(|| anyhow::anyhow!("cannot resolve `{rev}` to a commit"))?
        .as_str()
        .to_string();
    let short_sha = &full_sha[..full_sha.len().min(7)];

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
                        .strip_prefix(&repo)
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

    if entries.is_empty() {
        let short = &full_sha[..full_sha.len().min(7)];
        println!("no feedback for {short}");
        return Ok(());
    }

    if args.json {
        let json: Vec<FeedbackEntryJson> = entries.iter().map(FeedbackEntryJson::from).collect();
        println!("{}", serde_json::to_string_pretty(&json)?);
    } else {
        let short = &full_sha[..full_sha.len().min(7)];
        println!("commit {short}");
        for e in &entries {
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
            println!("  {}  {}{}", e.author, verdict_str, summary);
            if !e.details.is_empty() {
                for line in e.details.lines() {
                    println!("    {line}");
                }
            }
            println!("    {}", e.source_path);
        }
    }

    Ok(())
}

struct FeedbackEntry {
    author: String,
    verdict: clank_core::Verdict,
    summary: String,
    details: String,
    source_path: String,
}

/// `clank feedback read --json` row. Borrows from a
/// [`FeedbackEntry`]; `verdict` is the lowercase wire string (not
/// the typed enum's Serialize) to match the prior `json!` shape
/// (typed-json-not-json-macro).
#[derive(serde::Serialize)]
struct FeedbackEntryJson<'a> {
    author: &'a str,
    verdict: &'a str,
    summary: &'a str,
    details: &'a str,
    source_path: &'a str,
}

impl<'a> From<&'a FeedbackEntry> for FeedbackEntryJson<'a> {
    fn from(e: &'a FeedbackEntry) -> Self {
        Self {
            author: &e.author,
            verdict: e.verdict.as_str(),
            summary: &e.summary,
            details: &e.details,
            source_path: &e.source_path,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feedback_entry_json_matches_prior_shape() {
        // typed-json-not-json-macro: FeedbackEntryJson serializes to
        // the same keys+values the old `json!` produced. `verdict` is
        // the lowercase wire string, not the typed enum.
        let entry = FeedbackEntry {
            author: "codex".to_string(),
            verdict: clank_core::Verdict::RequestChanges,
            summary: "needs work".to_string(),
            details: "line one\nline two".to_string(),
            source_path: ".clank/agents/codex/feedback/abc.md".to_string(),
        };
        assert_eq!(
            serde_json::to_value(FeedbackEntryJson::from(&entry)).unwrap(),
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
