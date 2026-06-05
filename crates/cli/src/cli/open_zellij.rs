//! `clank open zellij` — auto-generate a zellij layout (KDL)
//! that spawns master + reviewers in tabbed panes via
//! `clank agent start`. Replaces the POC at `open-worktree.sh`'s
//! layout-generation portion.

use std::path::Path;

use anyhow::Context;
use clank_core::vocab::Role;

use super::OpenZellijArgs;
use super::config::{DefaultAgent, load_merged_agents};
use super::{repo_basename, resolve_repo};

pub async fn run(args: OpenZellijArgs) -> anyhow::Result<()> {
    let repo = resolve_repo(args.repo.as_deref())?;
    let basename = repo_basename(&repo)?;
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let agents = load_merged_agents(&repo, home.as_deref())
        .with_context(|| format!("loading agent declaration for `{}`", repo.display()))?;
    let (master, reviewers) = classify_roles(&agents)?;
    let kdl = compose_kdl(master, &reviewers);
    let spawn_argv = compose_spawn_argv(&repo, &basename, &kdl);

    if args.print {
        println!("{kdl}");
        eprintln!("spawn: {}", spawn_argv.join(" "));
        return Ok(());
    }

    // Sanity: require a zellij session in non-print mode.
    if std::env::var_os("ZELLIJ_SESSION_NAME").is_none() {
        anyhow::bail!(
            "not inside a zellij session (ZELLIJ_SESSION_NAME unset). \
             Run `clank open zellij` from a shell pane inside zellij, \
             or pass `--print` to emit the KDL without spawning."
        );
    }

    let status = std::process::Command::new("zellij")
        .arg("action")
        .arg("new-tab")
        .arg("--cwd")
        .arg(&repo)
        .arg("--name")
        .arg(&basename)
        .arg("--layout-string")
        .arg(&kdl)
        .status()
        .context("spawning zellij action new-tab")?;
    if !status.success() {
        anyhow::bail!("zellij action new-tab exited {status}");
    }
    Ok(())
}

/// Triage the declaration into (master, reviewers).
///
/// - zero masters → error (suggest `clank agent add ... --role master`)
/// - multiple masters → error (suggest `clank agent set-role`)
/// - exactly one master → ok; reviewers preserve declaration order
fn classify_roles(agents: &[DefaultAgent]) -> anyhow::Result<(&DefaultAgent, Vec<&DefaultAgent>)> {
    let masters: Vec<&DefaultAgent> = agents.iter().filter(|a| a.role == Role::Master).collect();
    match masters.len() {
        0 => anyhow::bail!(
            "no master agent registered for this repo — register one with \
             `clank agent add <label> --role master`"
        ),
        1 => {
            let master = masters[0];
            let reviewers: Vec<&DefaultAgent> =
                agents.iter().filter(|a| a.role == Role::Reviewer).collect();
            Ok((master, reviewers))
        }
        _ => {
            let names: Vec<String> = masters
                .iter()
                .map(|a| a.label.as_str().to_string())
                .collect();
            anyhow::bail!(
                "multiple master agents registered: {}. zellij layout requires \
                 exactly one master. Resolve with `clank agent set-role <label> reviewer` \
                 to demote a duplicate.",
                names.join(", ")
            )
        }
    }
}

/// Hand-rolled KDL composer. `AgentLabel::parse` restricts charset
/// so we don't need quote-escaping here.
fn compose_kdl(master: &DefaultAgent, reviewers: &[&DefaultAgent]) -> String {
    let mut out = String::new();
    out.push_str("layout {\n");
    out.push_str("    pane size=1 borderless=true {\n");
    out.push_str("        plugin location=\"zellij:tab-bar\"\n");
    out.push_str("    }\n");
    out.push_str("    pane split_direction=\"horizontal\" {\n");
    push_pane(&mut out, master.label.as_str(), "master");
    for reviewer in reviewers {
        push_pane(&mut out, reviewer.label.as_str(), "reviewer");
    }
    out.push_str("    }\n");
    out.push_str("    pane size=2 borderless=true {\n");
    out.push_str("        plugin location=\"zellij:status-bar\"\n");
    out.push_str("    }\n");
    out.push_str("}\n");
    out
}

fn push_pane(out: &mut String, label: &str, role_str: &str) {
    out.push_str(&format!("        pane name=\"{label} ({role_str})\" {{\n"));
    out.push_str("            command \"clank\"\n");
    out.push_str(&format!(
        "            args \"agent\" \"start\" \"{label}\"\n"
    ));
    out.push_str("        }\n");
}

/// The argv we would pass to `zellij action new-tab` — emitted on
/// stderr in `--print` mode so tests + tools can assert on tab
/// name / cwd / KDL without observing the spawn.
fn compose_spawn_argv(repo: &Path, basename: &str, kdl: &str) -> Vec<String> {
    vec![
        "zellij".to_string(),
        "action".to_string(),
        "new-tab".to_string(),
        "--cwd".to_string(),
        repo.display().to_string(),
        "--name".to_string(),
        basename.to_string(),
        "--layout-string".to_string(),
        kdl.to_string(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use clank_core::ids::AgentLabel;

    fn agent(label: &str, role: Role) -> DefaultAgent {
        DefaultAgent {
            label: AgentLabel::parse(label).unwrap(),
            role,
            tool: None,
            launch: None,
        }
    }

    #[test]
    fn classify_roles_zero_master_errors_with_suggestion() {
        let agents = vec![agent("alice", Role::Reviewer)];
        let err = classify_roles(&agents).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("no master") && msg.contains("clank agent add"),
            "diagnostic should name the fix; got: {msg}"
        );
    }

    #[test]
    fn classify_roles_multi_master_errors_with_both_labels() {
        let agents = vec![
            agent("alice", Role::Master),
            agent("bob", Role::Master),
            agent("carol", Role::Reviewer),
        ];
        let err = classify_roles(&agents).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("alice") && msg.contains("bob"),
            "multi-master diagnostic must list BOTH masters; got: {msg}"
        );
        assert!(
            msg.contains("clank agent set-role"),
            "diagnostic should suggest set-role; got: {msg}"
        );
    }

    #[test]
    fn classify_roles_single_master_returns_master_and_reviewers_in_order() {
        let agents = vec![
            agent("master", Role::Master),
            agent("bob", Role::Reviewer),
            agent("alice", Role::Reviewer),
            agent("codex", Role::Reviewer),
        ];
        let (master, reviewers) = classify_roles(&agents).unwrap();
        assert_eq!(master.label.as_str(), "master");
        let labels: Vec<&str> = reviewers.iter().map(|a| a.label.as_str()).collect();
        // Declaration order preserved (NOT alphabetical — codex came after alice).
        assert_eq!(labels, vec!["bob", "alice", "codex"]);
    }

    #[test]
    fn compose_kdl_includes_tab_bar_and_status_bar_plugins() {
        let master = agent("m", Role::Master);
        let kdl = compose_kdl(&master, &[]);
        assert!(kdl.contains("plugin location=\"zellij:tab-bar\""));
        assert!(kdl.contains("plugin location=\"zellij:status-bar\""));
    }

    #[test]
    fn compose_kdl_master_and_reviewer_panes_use_clank_agent_start() {
        let master = agent("alice", Role::Master);
        let bob = agent("bob", Role::Reviewer);
        let carol = agent("carol", Role::Reviewer);
        let kdl = compose_kdl(&master, &[&bob, &carol]);
        assert!(kdl.contains("name=\"alice (master)\""));
        assert!(kdl.contains("name=\"bob (reviewer)\""));
        assert!(kdl.contains("name=\"carol (reviewer)\""));
        assert!(kdl.contains("command \"clank\""));
        assert!(kdl.contains("args \"agent\" \"start\" \"alice\""));
        assert!(kdl.contains("args \"agent\" \"start\" \"bob\""));
        assert!(kdl.contains("args \"agent\" \"start\" \"carol\""));
    }

    #[test]
    fn compose_kdl_reviewer_order_matches_input_order() {
        let master = agent("m", Role::Master);
        let bob = agent("bob", Role::Reviewer);
        let alice = agent("alice", Role::Reviewer);
        let codex = agent("codex", Role::Reviewer);
        let kdl = compose_kdl(&master, &[&bob, &alice, &codex]);
        // Strip everything before the first reviewer to make the
        // ordering assertion clean.
        let bob_idx = kdl.find("name=\"bob (reviewer)\"").unwrap();
        let alice_idx = kdl.find("name=\"alice (reviewer)\"").unwrap();
        let codex_idx = kdl.find("name=\"codex (reviewer)\"").unwrap();
        assert!(bob_idx < alice_idx, "bob must come before alice");
        assert!(alice_idx < codex_idx, "alice must come before codex");
    }

    #[test]
    fn compose_spawn_argv_includes_cwd_and_name_and_layout_string() {
        let argv = compose_spawn_argv(Path::new("/tmp/repo"), "weirdname", "layout {}\n");
        assert!(argv.contains(&"--cwd".to_string()));
        assert!(argv.contains(&"/tmp/repo".to_string()));
        assert!(argv.contains(&"--name".to_string()));
        assert!(argv.contains(&"weirdname".to_string()));
        assert!(argv.contains(&"--layout-string".to_string()));
        assert!(argv.contains(&"layout {}\n".to_string()));
    }
}
