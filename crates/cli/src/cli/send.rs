//! `clank send`: put a message in an agent's inbox.

use std::path::{Path, PathBuf};

use clank_core::ids::AgentLabel;

use super::{SendArgs, resolve_repo};
use crate::agent_env::Ambient;

pub fn run(args: SendArgs) -> anyhow::Result<()> {
    let repo = resolve_repo(args.repo.as_deref())?;
    let to = recipient(&repo, args.to.as_deref(), args.master)?;
    let body = body_of(&args.message, args.file.as_deref())?;
    let from = sender_in(&crate::agent_env::Process);
    let path = crate::inbox::send(
        &repo,
        &to,
        args.priority,
        from.as_ref(),
        &crate::cli::stop_hook::now_rfc3339(),
        &body,
    )?;
    println!("sent to `{to}` in {}: {}", repo.display(), path.display());
    Ok(())
}

/// The label a message to `repo` goes to: `repo`'s master when
/// `master`, else `to`, which must be on `repo`'s roster.
pub(crate) fn recipient(repo: &Path, to: Option<&str>, master: bool) -> anyhow::Result<AgentLabel> {
    let Some(set) = crate::agent_store::try_resolve_via_team(repo)? else {
        anyhow::bail!(
            "`{}` has no roster to send to: it has no master agent",
            repo.display()
        );
    };
    if master {
        return Ok(set.master);
    }
    let raw = to.ok_or_else(|| anyhow::anyhow!("name a recipient: --to <label> or --master"))?;
    let label = AgentLabel::parse(raw).map_err(|e| anyhow::anyhow!("invalid --to `{raw}`: {e}"))?;
    if set.master != label && !set.reviewers.iter().any(|r| r.label == label) {
        anyhow::bail!(
            "`{label}` is not on `{}`'s roster ({})",
            repo.display(),
            crate::cli::agent::registered_labels(&set).join(", ")
        );
    }
    Ok(label)
}

/// The message: `-m` paragraphs joined as `git commit` joins them, or
/// a file's contents, `-` meaning stdin. Never empty.
fn body_of(message: &[String], file: Option<&Path>) -> anyhow::Result<String> {
    let body = match file {
        Some(p) if p == Path::new("-") => std::io::read_to_string(std::io::stdin())?,
        Some(p) => std::fs::read_to_string(p)
            .map_err(|e| anyhow::anyhow!("reading `{}`: {e}", p.display()))?,
        None => message.join("\n\n"),
    };
    if body.trim().is_empty() {
        anyhow::bail!("the message is empty");
    }
    Ok(body)
}

/// Who is sending: the agent this process runs as, in the repo it runs
/// in — what the recipient replies to. `None` when no agent is, which
/// is a person sending.
pub(crate) fn sender_in(env: &dyn Ambient) -> Option<crate::inbox::Sender> {
    let cwd = env.cwd()?;
    let here: PathBuf = crate::git_io::discover_work_dir(&cwd).ok()??;
    let here = dunce::canonicalize(&here).ok()?;
    let label = crate::agent_env::resolve_identity_in(&here, env).ok()?;
    Some(crate::inbox::Sender {
        label: label.as_str().to_string(),
        repo: here,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_env::Stated;

    fn repo_with_roster(roster: &str) -> tempfile::TempDir {
        let repo = tempfile::tempdir().unwrap();
        let init = std::process::Command::new("git")
            .arg("-C")
            .arg(repo.path())
            .args(["init", "--quiet", "--initial-branch=main"])
            .status()
            .unwrap();
        assert!(init.success());
        std::fs::create_dir_all(repo.path().join(".clank")).unwrap();
        std::fs::write(repo.path().join(".clank/config.json"), roster).unwrap();
        repo
    }

    const TEAM: &str = r#"{"agents":{"claude":{"tool":"claude","role":"master"},"codex":{"tool":"codex","role":"commit"}}}"#;

    #[test]
    fn master_is_whoever_the_roster_names() {
        let repo = repo_with_roster(TEAM);
        assert_eq!(
            recipient(repo.path(), None, true).unwrap().as_str(),
            "claude"
        );
        crate::cli::agent::set_repo_master(repo.path(), &AgentLabel::parse("codex").unwrap())
            .unwrap();
        assert_eq!(
            recipient(repo.path(), None, true).unwrap().as_str(),
            "codex",
            "`--master` follows a promote"
        );
    }

    #[test]
    fn a_label_off_the_roster_is_refused() {
        let repo = repo_with_roster(TEAM);
        assert_eq!(
            recipient(repo.path(), Some("codex"), false)
                .unwrap()
                .as_str(),
            "codex"
        );
        let err = recipient(repo.path(), Some("grok"), false).unwrap_err();
        assert!(err.to_string().contains("not on"), "{err}");
        assert!(err.to_string().contains("claude, codex"), "{err}");
    }

    #[test]
    fn a_message_sent_to_another_repo_lands_in_its_inbox() {
        let there = repo_with_roster(TEAM);
        let to = recipient(there.path(), None, true).unwrap();
        crate::inbox::send(
            there.path(),
            &to,
            500,
            None,
            "2026-09-22T10:15:00Z",
            "hello",
        )
        .unwrap();
        let pending = crate::inbox::scan(there.path(), &to);
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].subject(), "hello");
    }

    #[test]
    fn an_agent_sends_as_itself_and_a_person_as_nobody() {
        let repo = repo_with_roster(TEAM);
        let here = dunce::canonicalize(repo.path()).unwrap();
        let home = tempfile::tempdir().unwrap();

        let agent = Stated::saying(&[("CLANK_AGENT", "codex")]).at(home.path(), repo.path());
        assert_eq!(
            sender_in(&agent),
            Some(crate::inbox::Sender {
                label: "codex".to_string(),
                repo: here,
            })
        );
        let person = Stated::default().at(home.path(), repo.path());
        assert_eq!(sender_in(&person), None);
    }

    #[test]
    fn a_send_names_one_recipient_and_one_body() {
        use clap::Parser;
        let parse = |args: &[&str]| {
            crate::cli::command::Cli::try_parse_from(
                std::iter::once("clank").chain(args.iter().copied()),
            )
            .is_ok()
        };
        assert!(parse(&["send", "--to", "codex", "-m", "hi"]));
        assert!(parse(&["send", "--master", "-m", "one", "-m", "two"]));
        assert!(parse(&["send", "--master", "-F", "-"]));
        assert!(!parse(&["send", "-m", "hi"]), "no recipient");
        assert!(
            !parse(&["send", "--to", "codex", "--master", "-m", "hi"]),
            "two recipients"
        );
        assert!(!parse(&["send", "--master"]), "no body");
        assert!(
            !parse(&["send", "--master", "-m", "hi", "-F", "x"]),
            "two bodies"
        );
        assert!(
            parse(&["inbox", "done", "x", "--author", "grok"]),
            "--author after the verb"
        );
        assert!(parse(&["inbox", "--author", "grok"]), "list is the default");
    }

    #[test]
    fn paragraphs_join_and_an_empty_message_is_refused() {
        assert_eq!(
            body_of(&["one".into(), "two".into()], None).unwrap(),
            "one\n\ntwo"
        );
        assert!(body_of(&["  ".into()], None).is_err());
    }
}
