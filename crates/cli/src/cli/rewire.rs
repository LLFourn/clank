//! `clank rewire --from-stdin` — copy feedback files forward
//! after a rebase. Driven by the `post-rewrite` git hook clank
//! installs in `.git/hooks/post-rewrite`.

use std::io::Read;
use std::path::{Path, PathBuf};

use clank_core::ids::{AgentLabel, CommitSha};

use super::{RewireArgs, resolve_repo};
use crate::disk_format::canonical_feedback_path;

pub async fn run(args: RewireArgs) -> anyhow::Result<()> {
    if !args.from_stdin {
        anyhow::bail!("`clank rewire` currently requires --from-stdin");
    }
    let repo = resolve_repo(args.repo.as_deref())?;
    let mut input = String::new();
    std::io::stdin()
        .read_to_string(&mut input)
        .map_err(|e| anyhow::anyhow!("reading stdin: {e}"))?;
    let pairs = parse_pairs(&input);
    let copied = apply_pairs(&repo, &pairs)?;
    if copied > 0 {
        eprintln!(
            "clank rewire: copied feedback for {copied} commit{}",
            if copied == 1 { "" } else { "s" }
        );
    }
    Ok(())
}

/// Copy feedback for each `(old, new)` sha pair. Shared by the `post-rewrite`
/// hook path ([`run`]) and in-process rewriters.
fn apply_pairs(repo: &Path, pairs: &[RewritePair]) -> anyhow::Result<usize> {
    let plan = plan_actions(repo, pairs);
    let mut copied = 0usize;
    for action in &plan.actions {
        if copy_file(action)? {
            copied += 1;
        }
    }
    Ok(copied)
}

/// Migrate review feedback from each `old` sha to its `new` sha after an
/// in-process history rewrite (e.g. `finish -m` rewording a buried finalize
/// and replaying the work stacked on top). The git `post-rewrite` hook only
/// fires for `git` rebase/amend, so a rewrite done via plumbing (`update-ref`)
/// must call this to keep sha-keyed feedback attached to its commit.
pub(crate) fn migrate_feedback_pairs(
    repo: &Path,
    pairs: &[(CommitSha, CommitSha)],
) -> anyhow::Result<usize> {
    let pairs: Vec<RewritePair> = pairs
        .iter()
        .map(|(old, new)| RewritePair {
            old: old.clone(),
            new: new.clone(),
        })
        .collect();
    apply_pairs(repo, &pairs)
}

/// One stdin line, parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RewritePair {
    old: CommitSha,
    new: CommitSha,
}

fn parse_pairs(input: &str) -> Vec<RewritePair> {
    let mut out = Vec::new();
    for line in input.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split_whitespace();
        let Some(old) = parts.next() else {
            continue;
        };
        let Some(new) = parts.next() else {
            continue;
        };
        let Ok(old) = CommitSha::parse(old) else {
            continue;
        };
        let Ok(new) = CommitSha::parse(new) else {
            continue;
        };
        out.push(RewritePair { old, new });
    }
    out
}

#[derive(Debug)]
struct CopyAction {
    src: PathBuf,
    dst: PathBuf,
}

struct Plan {
    actions: Vec<CopyAction>,
}

fn plan_actions(repo: &Path, pairs: &[RewritePair]) -> Plan {
    // Group by new sha; within each group keep the LAST old in input
    // order (most recent commit in the rebase processing sequence).
    let mut latest_old_per_new: Vec<(CommitSha, CommitSha)> = Vec::new();
    for p in pairs {
        if let Some(slot) = latest_old_per_new.iter_mut().find(|(_, new)| new == &p.new) {
            slot.0 = p.old.clone();
        } else {
            latest_old_per_new.push((p.old.clone(), p.new.clone()));
        }
    }

    // Build the scope_shas the writer uses for canonical paths.
    // Best-effort fold; if it fails we degrade to writing full
    // form so the reader's full-first lookup still finds the file.
    let scope_shas = scope_shas_via_fold(repo);

    let agents_dir = repo.join(".clank/agents");
    let mut actions = Vec::new();
    let Ok(read) = std::fs::read_dir(&agents_dir) else {
        return Plan { actions };
    };
    let mut author_dirs: Vec<(AgentLabel, PathBuf)> = Vec::new();
    for entry in read.flatten() {
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        let Ok(label) = AgentLabel::parse(name_str) else {
            continue;
        };
        let feedback_dir = entry.path().join("feedback");
        if !feedback_dir.is_dir() {
            continue;
        }
        author_dirs.push((label, feedback_dir));
    }

    for (latest_old, new) in &latest_old_per_new {
        for (label, feedback_dir) in &author_dirs {
            let Some(src) = find_source(feedback_dir, latest_old) else {
                continue;
            };
            let dst_rel = canonical_feedback_path(label, new, &scope_shas);
            // canonical_feedback_path returns `agents/<label>/feedback/<ref>.md`;
            // make it absolute under `.clank/`.
            let dst = repo.join(".clank").join(&dst_rel);
            actions.push(CopyAction { src, dst });
        }
    }

    Plan { actions }
}

/// Probe for the source file in the same order
/// `FsPlanStateLookup::reviews_for` does: full-form first, then
/// 7-char short form.
fn find_source(feedback_dir: &Path, old: &CommitSha) -> Option<PathBuf> {
    let full = feedback_dir.join(format!("{}.md", old.as_str()));
    if full.is_file() {
        return Some(full);
    }
    let short = &old.as_str()[..7.min(old.as_str().len())];
    let short_path = feedback_dir.join(format!("{short}.md"));
    if short_path.is_file() {
        return Some(short_path);
    }
    None
}

fn copy_file(action: &CopyAction) -> anyhow::Result<bool> {
    if let Some(parent) = action.dst.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| anyhow::anyhow!("creating `{}`: {e}", parent.display()))?;
    }
    std::fs::copy(&action.src, &action.dst).map_err(|e| {
        anyhow::anyhow!(
            "copying `{}` → `{}`: {e}",
            action.src.display(),
            action.dst.display(),
        )
    })?;
    Ok(true)
}

/// Best-effort: fold the repo to compute scope_shas the same way
/// `cli/feedback.rs::run_write` does. Returns an empty vec on
/// any failure so the canonical path falls back to short form
/// (which the reader still finds via its short-fallback path).
fn scope_shas_via_fold(repo: &Path) -> Vec<CommitSha> {
    let rt = match tokio::runtime::Handle::try_current() {
        Ok(h) => h,
        Err(_) => return Vec::new(),
    };
    let repo_path = repo.to_path_buf();
    let fold = tokio::task::block_in_place(|| {
        rt.block_on(async move {
            crate::rebuild::rebuild_repo_with_policy(&repo_path, crate::rebuild::CachePolicy::Use)
                .await
        })
    });
    let Ok(state) = fold else {
        return Vec::new();
    };
    let mut all_shas: Vec<CommitSha> = Vec::new();
    for ps in state.fold.plans.values() {
        all_shas.extend(ps.reviewable_shas());
    }
    for ah in &state.fold.ad_hoc {
        all_shas.push(ah.sha.clone());
    }
    all_shas.sort();
    all_shas.dedup();
    all_shas
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sha_full(c: char) -> CommitSha {
        CommitSha::parse(&c.to_string().repeat(40)).unwrap()
    }

    fn make_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".clank/agents/alice/feedback")).unwrap();
        dir
    }

    fn write(p: &Path, body: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    #[test]
    fn parses_post_rewrite_lines() {
        let input = "\
0000000000000000000000000000000000000000 1111111111111111111111111111111111111111
2222222222222222222222222222222222222222 3333333333333333333333333333333333333333 extra-fields
\n";
        let pairs = parse_pairs(input);
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0].old.as_str(), &"0".repeat(40));
        assert_eq!(pairs[1].new.as_str(), &"3".repeat(40));
    }

    #[test]
    fn plan_groups_squash_keeping_latest_old() {
        let dir = make_repo();
        let old_a = sha_full('a');
        let old_b = sha_full('b');
        let old_c = sha_full('c');
        let new = sha_full('d');
        // Order matters: a is processed first, c last.
        let pairs = vec![
            RewritePair {
                old: old_a.clone(),
                new: new.clone(),
            },
            RewritePair {
                old: old_b.clone(),
                new: new.clone(),
            },
            RewritePair {
                old: old_c.clone(),
                new: new.clone(),
            },
        ];
        // Seed feedback ONLY on old_a (the earliest squashed
        // commit). Expected: no copy emitted, because the LAST
        // old (old_c) has no source file.
        write(
            &dir.path().join(format!(
                ".clank/agents/alice/feedback/{}.md",
                old_a.as_str()
            )),
            "CONTINUE\n",
        );
        let plan = plan_actions(dir.path(), &pairs);
        assert!(
            plan.actions.is_empty(),
            "expected no actions; got {:?}",
            plan.actions
        );
    }

    #[test]
    fn plan_squash_copies_when_latest_old_has_feedback() {
        let dir = make_repo();
        let old_a = sha_full('a');
        let old_b = sha_full('b');
        let new = sha_full('d');
        let pairs = vec![
            RewritePair {
                old: old_a.clone(),
                new: new.clone(),
            },
            RewritePair {
                old: old_b.clone(),
                new: new.clone(),
            },
        ];
        write(
            &dir.path().join(format!(
                ".clank/agents/alice/feedback/{}.md",
                old_b.as_str()
            )),
            "CONTINUE bob\n",
        );
        let plan = plan_actions(dir.path(), &pairs);
        assert_eq!(plan.actions.len(), 1);
        assert!(
            plan.actions[0]
                .dst
                .to_string_lossy()
                .contains(&new.as_str()[..7])
                || plan.actions[0].dst.to_string_lossy().contains(new.as_str()),
            "dst path should include some form of new sha: {}",
            plan.actions[0].dst.display()
        );
    }

    #[test]
    fn plan_simple_rename_copies_feedback() {
        let dir = make_repo();
        let old = sha_full('a');
        let new = sha_full('b');
        let pairs = vec![RewritePair {
            old: old.clone(),
            new: new.clone(),
        }];
        write(
            &dir.path()
                .join(format!(".clank/agents/alice/feedback/{}.md", old.as_str())),
            "CONTINUE\n",
        );
        let plan = plan_actions(dir.path(), &pairs);
        assert_eq!(plan.actions.len(), 1);
        assert!(plan.actions[0].src.is_file());
    }

    #[test]
    fn plan_source_short_form_resolves() {
        let dir = make_repo();
        let old = sha_full('a');
        let new = sha_full('b');
        let pairs = vec![RewritePair {
            old: old.clone(),
            new: new.clone(),
        }];
        // Source stored as 7-char short form.
        let short = &old.as_str()[..7];
        write(
            &dir.path()
                .join(format!(".clank/agents/alice/feedback/{short}.md")),
            "CONTINUE\n",
        );
        let plan = plan_actions(dir.path(), &pairs);
        assert_eq!(plan.actions.len(), 1);
        assert!(
            plan.actions[0].src.to_string_lossy().contains(short),
            "src should be the short-form file: {}",
            plan.actions[0].src.display()
        );
    }

    #[test]
    fn plan_skips_when_no_source_feedback_exists() {
        let dir = make_repo();
        let old = sha_full('a');
        let new = sha_full('b');
        let pairs = vec![RewritePair { old, new }];
        // No feedback file written.
        let plan = plan_actions(dir.path(), &pairs);
        assert!(plan.actions.is_empty());
    }

    #[test]
    fn plan_handles_multiple_authors() {
        let dir = make_repo();
        std::fs::create_dir_all(dir.path().join(".clank/agents/bob/feedback")).unwrap();
        let old = sha_full('a');
        let new = sha_full('b');
        let pairs = vec![RewritePair {
            old: old.clone(),
            new,
        }];
        write(
            &dir.path()
                .join(format!(".clank/agents/alice/feedback/{}.md", old.as_str())),
            "CONTINUE\n",
        );
        write(
            &dir.path()
                .join(format!(".clank/agents/bob/feedback/{}.md", old.as_str())),
            "FINISHED\n",
        );
        let plan = plan_actions(dir.path(), &pairs);
        assert_eq!(plan.actions.len(), 2);
        let labels: Vec<String> = plan
            .actions
            .iter()
            .filter_map(|a| {
                a.dst
                    .components()
                    .rev()
                    .nth(2)
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
            })
            .collect();
        assert!(labels.iter().any(|l| l == "alice"));
        assert!(labels.iter().any(|l| l == "bob"));
    }

    #[test]
    fn migrate_feedback_pairs_copies_forward() {
        // The in-process entry point (used by `finish -m` off-head reword)
        // migrates feedback to the new sha, same as the hook path.
        let dir = make_repo();
        let old = sha_full('a');
        let new = sha_full('b');
        write(
            &dir.path()
                .join(format!(".clank/agents/alice/feedback/{}.md", old.as_str())),
            "CONTINUE\n",
        );
        let copied = migrate_feedback_pairs(dir.path(), &[(old, new.clone())]).unwrap();
        assert_eq!(copied, 1);
        let short = &new.as_str()[..7];
        let full = dir
            .path()
            .join(format!(".clank/agents/alice/feedback/{}.md", new.as_str()));
        let short_p = dir
            .path()
            .join(format!(".clank/agents/alice/feedback/{short}.md"));
        assert!(
            full.is_file() || short_p.is_file(),
            "feedback must be migrated to the new sha"
        );
    }

    #[test]
    fn plan_noop_when_agents_dir_missing() {
        let dir = tempfile::tempdir().unwrap();
        let pairs = vec![RewritePair {
            old: sha_full('a'),
            new: sha_full('b'),
        }];
        let plan = plan_actions(dir.path(), &pairs);
        assert!(plan.actions.is_empty());
    }
}
