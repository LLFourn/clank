//! Enforcement gate (the-stop-hook-is-a-messenger): the provider hooks
//! carry evidence in and verdicts out, and the monitor owns every decision
//! about an agent's turn-ends and its recovery. So the production code of
//! every hook-side path — the Stop and SessionStart adapter, Grok's native
//! adapter, recovery's transport, the owner lifetime they share, and the
//! child that carries a report, Alone fallback included — must neither
//! read nor write the files that hold those decisions, nor make them
//! itself: no auto-mode or role resolution, no recovery transport choice,
//! no record or episode writes.
//!
//! An unavailable monitor yields a transport diagnostic, never local policy.
//! Shared binding helpers are checked separately below; broker code executes
//! in the broker, not in the thin question adapter.

use crate::common::{Production, crates_dir, rs_files, strip_test_code};

/// The hook-side modules.
const HOOK_FILES: &[&str] = &[
    "stop_hook.rs",
    "grok_hook.rs",
    "recovery_hook.rs",
    "hook_owner.rs",
    "hook_handoff.rs",
    "question_hook.rs",
    "turn_child.rs",
];

/// What (if anything) a production line of a hook module touches.
fn line_violation(line: &str) -> Option<&'static str> {
    const FILES: &[(&str, &str)] = &[
        ("\"attending\"", "the attendance marker"),
        ("\"attended\"", "the attendance projection"),
        ("attending.holder", "the check-in holder"),
        ("repeat.json", "the repeat count"),
        ("repeat.lock", "the repeat lock"),
        ("question.json", "the question state"),
        ("question.lock", "the question lock"),
        ("recovery.json", "the recovery episode"),
        ("recovery-issued.json", "recovery's issued attempts"),
        ("recovery-reply.json", "a recovery reply"),
        ("turns.jsonl", "a harness's observed turns"),
        ("turn.json", "the monitor's turn state"),
        ("state.json", "the monitor's published state"),
    ];
    const APIS: &[(&str, &str)] = &[
        ("question_state::observe", "a question-state write"),
        ("question_state::load", "a question-state read"),
        ("question_state::asking", "a question-state read"),
        ("question_state::path", "the question-state file"),
        ("recovery::observe(", "a write of a harness's record"),
        ("recovery::store(", "a recovery episode write"),
        ("recovery::load(", "a recovery episode read"),
        ("observed_path(", "a harness's record"),
        ("episode_path(", "the recovery episode"),
        ("pending_path(", "recovery's pending attempts"),
        ("nudge_log::", "the nudge history"),
        ("transport_for(", "recovery's transport choice"),
        ("retry_announcement(", "recovery's transport choice"),
        ("resolve_effective_auto_mode", "an auto-mode decision"),
        (
            "resolve_identity_from_env",
            "the general CLI config resolver",
        ),
        ("Gathered::", "monitor fact gathering in a hook process"),
        ("unmonitored_verdict", "local outage policy"),
        ("announce_outage", "local outage wake authorization"),
        ("outage_served", "monitor recovery state"),
        ("load_agent_config", "an agent's config, read for policy"),
        ("AutoMode::", "an auto-mode decision"),
        ("resolve_role", "a role decision"),
        ("agent_state::", "the monitor's published state"),
        ("agents_root(", "an agent's directory"),
        ("Turns::new(", "the monitor's turn custody"),
    ];
    FILES
        .iter()
        .chain(APIS)
        .find(|(needle, _)| line.contains(needle))
        .map(|(_, what)| *what)
}

#[test]
fn hook_modules_touch_no_decision_state() {
    let mut files = Vec::new();
    rs_files(&crates_dir().join("cli/src/cli"), &mut files);
    let production = Production::of(&files);
    let hooks: Vec<_> = files
        .iter()
        .filter(|f| {
            f.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| HOOK_FILES.contains(&n))
        })
        .collect();
    assert_eq!(
        hooks.len(),
        HOOK_FILES.len(),
        "every hook module is scanned: {hooks:?}"
    );
    let mut violations = Vec::new();
    for f in hooks {
        for (i, line) in production.text(f).lines().enumerate() {
            if let Some(what) = line_violation(line) {
                violations.push(format!("{}:{} — {what}", f.display(), i + 1));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "a hook module touches state the monitor owns ({} site(s)). Report the evidence \
         to the monitor and frame its verdict instead:\n  {}",
        violations.len(),
        violations.join("\n  ")
    );
}

#[test]
fn scanner_flags_prod_and_exempts_test_code() {
    let count = |src: &str| -> usize {
        strip_test_code(src)
            .lines()
            .filter(|l| line_violation(l).is_some())
            .count()
    };
    assert_eq!(count("fn f(d: &Path) { d.join(\"attending\"); }\n"), 1);
    assert_eq!(count("fn f() { question_state::observe(&d, []); }\n"), 1);
    assert_eq!(
        count("fn f() { crate::cli::recovery::observe(&d, &turn); }\n"),
        1
    );
    assert_eq!(
        count("fn f() { team::resolve_effective_auto_mode(cfg.as_ref(), home) }\n"),
        1
    );
    assert_eq!(
        count("fn f() { crate::agent_store::resolve_role(repo, &label)?; }\n"),
        1
    );
    assert_eq!(
        count("fn f() { recovery::transport_for(tool, Action::Retry, None) }\n"),
        1
    );
    assert_eq!(count("fn f() { agents_root(repo).join(\"x\"); }\n"), 1);
    assert_eq!(
        count(
            "fn f() { question_state::prompts_behind(t, p); recovery_hook::run(a); \
             recovery::opencode_class(n, s); binding_matches(r, l, t, s); }\n"
        ),
        0,
        "evidence, the transport and the routing binding check are not decisions"
    );
    let test_mod = "#[cfg(test)]\nmod tests {\n    fn f() { std::fs::write(d.join(\"attending\"), \"\"); }\n}\n";
    assert_eq!(count(test_mod), 0, "test code is exempt");
}

#[test]
fn shared_hook_identity_paths_cannot_parse_monitor_policy() {
    let root = crates_dir().join("cli/src");
    let identity =
        strip_test_code(&std::fs::read_to_string(root.join("agent_store/identity.rs")).unwrap());
    for forbidden in [
        "AgentConfig",
        "resolve_role",
        "resolve_effective_auto_mode",
        "try_resolve_via_team",
        "load_repo_config",
    ] {
        assert!(
            !identity.contains(forbidden),
            "binding projection uses {forbidden}"
        );
    }
    for (file, names) in [
        (
            "agent_env.rs",
            vec![
                "resolve_identity_for_hook_in",
                "bound_agent",
                "session_start",
            ],
        ),
        (
            "agent_store.rs",
            vec![
                "binding_in",
                "binding_matches",
                "bind_session_to_agent",
                "release_binding",
                "resume_checked_in",
                "record_transcript",
            ],
        ),
    ] {
        let src = std::fs::read_to_string(root.join(file)).unwrap();
        for name in names {
            let start = src.find(&format!("fn {name}(")).unwrap();
            let end = src[start..].find("\n}").unwrap() + start;
            let body = &src[start..end];
            for forbidden in [
                "AgentConfig",
                "load_agent_config",
                "load_all_agent_configs",
                "mutate_locked",
                "resolve_role",
                "resolve_tool",
            ] {
                assert!(!body.contains(forbidden), "{file}::{name} uses {forbidden}");
            }
        }
    }
}

#[test]
fn opencode_plugin_carries_native_events_without_config_access() {
    let src =
        std::fs::read_to_string(crates_dir().join("cli/src/cli/setup_assets/opencode_plugin.js"))
            .unwrap();
    for forbidden in [
        "node:fs",
        "readFile",
        "config.json",
        "auto_mode",
        "question.json",
        "state.json",
    ] {
        assert!(!src.contains(forbidden), "OpenCode plugin uses {forbidden}");
    }
}
