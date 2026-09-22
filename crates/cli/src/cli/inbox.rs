//! `clank inbox`: the calling agent's messages.

use clank_core::ids::AgentLabel;

use super::{InboxArgs, InboxCmd, resolve_repo};
use crate::inbox::Message;

pub fn run(args: InboxArgs) -> anyhow::Result<()> {
    let repo = resolve_repo(args.repo.as_deref())?;
    let label = match args.author.as_deref() {
        Some(raw) => {
            AgentLabel::parse(raw).map_err(|e| anyhow::anyhow!("invalid --author `{raw}`: {e}"))?
        }
        None => crate::agent_env::resolve_identity_from_env(&repo)?,
    };
    match args.command.unwrap_or(InboxCmd::List) {
        InboxCmd::List => {
            let messages = crate::inbox::scan(&repo, &label);
            if messages.is_empty() {
                println!("`{label}`'s inbox is empty");
            }
            for m in &messages {
                println!("{}", list_line(m));
            }
        }
        InboxCmd::Show { name } => {
            print!("{}", shown(&crate::inbox::resolve(&repo, &label, &name)?))
        }
        InboxCmd::Done { name } => {
            crate::inbox::done(&repo, &label, &name)?;
            println!("handled `{name}`");
        }
        InboxCmd::Reprioritise { name, priority } => {
            let path = crate::inbox::set_priority(&repo, &label, &name, priority)?;
            println!(
                "`{name}` now at priority {priority:03} ({})",
                path.display()
            );
        }
    }
    Ok(())
}

fn from_text(m: &Message) -> String {
    let repo = m.repo.as_ref().map(|r| r.display().to_string());
    crate::inbox::sender_words(m.from.as_deref(), repo.as_deref())
}

/// One listed message, led by its file stem: the handle `show` and
/// `done` resolve first, and the only one unique in the directory — a
/// parsed name is not (`100-500-note` and `500-note` both parse to a
/// name a stem can shadow).
fn list_line(m: &Message) -> String {
    format!("{}  {}  {}", m.entry.file_stem(), from_text(m), m.subject())
}

fn shown(m: &Message) -> String {
    let mut out = format!("{}\n", from_text(m));
    if let Some(sent) = &m.sent {
        out.push_str(&format!("sent {sent}\n"));
    }
    out.push('\n');
    out.push_str(&m.body);
    if !m.body.ends_with('\n') {
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_listing_names_the_sender_and_the_first_line() {
        let repo = tempfile::tempdir().unwrap();
        let grok = AgentLabel::parse("grok").unwrap();
        let claude = crate::inbox::Sender {
            label: "claude".to_string(),
            repo: "/src/clank".into(),
        };
        crate::inbox::send(
            repo.path(),
            &grok,
            200,
            Some(&claude),
            "2026-09-22T10:15:00Z",
            "# Rebase please\n\nonto master",
        )
        .unwrap();
        crate::inbox::send(
            repo.path(),
            &grok,
            500,
            None,
            "2026-09-22T10:16:00Z",
            "hello",
        )
        .unwrap();
        let lines: Vec<String> = crate::inbox::scan(repo.path(), &grok)
            .iter()
            .map(list_line)
            .collect();
        assert_eq!(
            lines,
            [
                "200-00000001-rebase-please  from claude (/src/clank)  Rebase please",
                "500-00000002-hello  from a person  hello",
            ]
        );
    }

    /// Every handle the listing shows resolves back to the message it
    /// was shown beside — including hand-written names a parsed name
    /// would let one file's stem shadow another's.
    #[test]
    fn every_listed_handle_resolves_to_its_own_message() {
        let repo = tempfile::tempdir().unwrap();
        let grok = AgentLabel::parse("grok").unwrap();
        let dir = crate::inbox::inbox_dir(repo.path(), &grok);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("100-500-note.md"), "Urgent first message").unwrap();
        std::fs::write(dir.join("500-note.md"), "Separate second message").unwrap();
        crate::inbox::send(
            repo.path(),
            &grok,
            500,
            None,
            "2026-09-22T10:15:00Z",
            "note",
        )
        .unwrap();

        for m in crate::inbox::scan(repo.path(), &grok) {
            let line = list_line(&m);
            let handle = line.split_whitespace().next().unwrap();
            let resolved = crate::inbox::resolve(repo.path(), &grok, handle).unwrap();
            assert_eq!(resolved.entry.path, m.entry.path, "{line}");
        }
    }

    #[test]
    fn showing_a_message_prints_its_sender_time_and_body() {
        let repo = tempfile::tempdir().unwrap();
        let grok = AgentLabel::parse("grok").unwrap();
        crate::inbox::send(
            repo.path(),
            &grok,
            500,
            None,
            "2026-09-22T10:15:00Z",
            "line one\nline two",
        )
        .unwrap();
        let m = crate::inbox::resolve(repo.path(), &grok, "00000001-line-one").unwrap();
        assert_eq!(
            shown(&m),
            "from a person\nsent 2026-09-22T10:15:00Z\n\nline one\nline two\n"
        );
    }
}
