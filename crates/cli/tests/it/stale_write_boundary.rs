//! Enforcement gate (no-process-outlives-its-build): every PRODUCTION
//! function in `crates/cli/src` that changes a file goes through the
//! upgrade authority, or is declared here with the reason it need not.
//!
//! A process that has seen a newer clank installed, and is still up (a
//! monitor settling what it handed out, a TUI hosting a remote, a wait
//! whose exec failed), must begin no new write of shared state. That
//! held only where a writer happened to ask, and the ones that did not
//! (an agent's config, recovery's episodes, the ledger) were found by
//! review, one at a time. This makes the next one a build failure.
//!
//! A function passes by naming the authority: it calls
//! `upgrade::writing()`, asks an `Authority` for a `permit()`, or takes
//! a `Permit` its caller already holds, which is how the record of a
//! delivery in flight is still written. Anything else that writes is in
//! [`UNGATED`], with why.
//!
//! One writer is outside this scan on purpose. The remote's door, in
//! `clank-web-cloudflare`, keeps the login sessions of whoever is
//! connected (`last_seen`, expiry) on every admitted request. A TUI
//! stays up on a replaced build for the sake of exactly that
//! connection, so refusing those writes would end what the process was
//! kept alive to preserve. Their files are fixture-pinned formats
//! (`remote-sessions.json`, `remote-links.json`).

use crate::common::{Production, crates_dir, is_cfg_test, rs_files};
use syn::visit::Visit;

/// What changing a file looks like in this codebase.
const WRITES: &[&str] = &[
    "fs::write(",
    "fs::rename(",
    "fs::remove_file(",
    "fs::remove_dir_all(",
    "fs::copy(",
    "File::create(",
    "File::options(",
    "OpenOptions::",
    "tmp.persist(",
    "temp.persist(",
    "staged.persist(",
    ".set_len(",
    "versioned::write(",
    "clank_user_config::transact(",
];

/// What going through the authority looks like.
const ASKS: &[&str] = &["upgrade::writing()", ".permit()", "Permit"];

/// Production functions that change a file without asking, as (path
/// under `crates/cli/src/`, function or `*` for every one in the file,
/// why). Three reasons recur:
///
/// - ONE-SHOT: the function runs only in a command that ends by itself
///   and watches no installation. Where the TUI or its remote can reach
///   the same change, they reach it through a gated helper named in the
///   entry.
/// - OWN: what it writes is this process's own (a lock, a socket, a
///   lease, its descriptor, its handoff), keyed to it and meaningless
///   once it is gone; no other build's process reads it as shared state.
/// - CALLER: it is only called by a function that asked.
const UNGATED: &[(&str, &str, &str)] = &[
    (
        "agent_store.rs",
        "exclusive_lock",
        "OWN: opens the label's lock file, which is never written",
    ),
    (
        "agent_store.rs",
        "bind_session_to_agent",
        "OWN: opens the roster lock file; the binding itself is saved through save_json, which asks",
    ),
    (
        "cli/agent.rs",
        "edit_roster",
        "OWN: opens the roster lock file; the roster is saved through repo_config, which asks",
    ),
    (
        "cli/block.rs",
        "run_migrate",
        "ONE-SHOT: `clank block migrate`",
    ),
    (
        "cli/claude_trust.rs",
        "*",
        "ONE-SHOT: launch-time trust of Claude's own config, under its own lock",
    ),
    (
        "cli/docs.rs",
        "*",
        "ONE-SHOT: `clank docs` writes the reference it was asked for",
    ),
    (
        "cli/feedback.rs",
        "write_atomic",
        "ONE-SHOT: `clank feedback write`",
    ),
    ("cli/finish.rs", "*", "ONE-SHOT: `clank finish`"),
    (
        "cli/fork.rs",
        "*",
        "ONE-SHOT: `clank fork`, and the launch that takes its spec",
    ),
    (
        "cli/github/log.rs",
        "acquire",
        "OWN: opens the log's lock file",
    ),
    (
        "cli/github/log.rs",
        "taking",
        "OWN: opens the ingest lease file",
    ),
    (
        "cli/github/log.rs",
        "load",
        "quarantines a log no build can parse, so that it is read once; it writes no row",
    ),
    (
        "cli/grok_launch.rs",
        "lock",
        "OWN: opens a launch lock file",
    ),
    (
        "cli/hook_handoff.rs",
        "*",
        "OWN: a hook's or a status watch's handoff to the build it becomes",
    ),
    (
        "cli/html.rs",
        "*",
        "ONE-SHOT: `clank html` writes the site it was asked for",
    ),
    ("cli/init.rs", "*", "ONE-SHOT: `clank init`"),
    (
        "cli/monitor.rs",
        "bind_listener_with",
        "OWN: the monitor's socket",
    ),
    (
        "cli/monitor.rs",
        "acquire_lease_taking",
        "OWN: the monitor's lease",
    ),
    (
        "cli/monitor.rs",
        "publish_in",
        "OWN: the monitor's descriptor, published only while it holds the lease as owner",
    ),
    (
        "cli/monitor.rs",
        "start",
        "OWN: removes this monitor's own installation observation as it leaves",
    ),
    (
        "cli/monitor.rs",
        "note_observation",
        "OWN: what this monitor sees of its installation, keyed to its pid: the record that it is stale",
    ),
    (
        "cli/monitor.rs",
        "forget_dead_observation",
        "OWN: observations of monitors that are gone, removed at startup",
    ),
    (
        "cli/source_diagnostic.rs",
        "terminal",
        "OWN: opens the terminal",
    ),
    (
        "cli/user_command/store.rs",
        "read_record",
        "opens a record to read it, refusing a symlink; writes nothing",
    ),
    (
        "cli/command_source.rs",
        "retire_after",
        "OWN: removes the record of a producer this process has just ended, which a stale monitor's stand-down must still do",
    ),
    (
        "cli/monitor.rs",
        "sweep_dead_sockets",
        "OWN: sockets of monitors that are gone",
    ),
    (
        "cli/open_zellij.rs",
        "write_layout_file",
        "ONE-SHOT: `clank open` writes its zellij layout",
    ),
    (
        "cli/owned_callback.rs",
        "*",
        "OWN: the record of a callback process this one started, so it can be ended",
    ),
    (
        "cli/pane_input.rs",
        "reserve_in",
        "OWN: a reservation file for one typed input, removed with it",
    ),
    (
        "cli/plan_mutation.rs",
        "runtime_lock",
        "OWN: opens a lock file",
    ),
    (
        "cli/remote_control.rs",
        "*",
        "OWN: this TUI's answer to a `clank remote` request, in its runtime directory",
    ),
    ("cli/rewire.rs", "*", "ONE-SHOT: the post-rewrite hook"),
    ("cli/setup.rs", "*", "ONE-SHOT: `clank setup`"),
    ("cli/stash.rs", "*", "ONE-SHOT: `clank stash`"),
    (
        "cli/status_watch.rs",
        "take",
        "OWN: removes the handoff this watch was resumed from",
    ),
    (
        "cli/status_tui/lease.rs",
        "*",
        "OWN: the TUI's lease on its repo",
    ),
    (
        "cli/status_tui/remote_owner.rs",
        "sweep",
        "OWN: requests left for TUIs that are gone",
    ),
    ("cli/term.rs", "winsize_of_tty", "OWN: opens the terminal"),
    (
        "cli/user_command/store.rs",
        "open_lock",
        "OWN: opens the store's lock file",
    ),
    (
        "cli/wake_receipt.rs",
        "acquire",
        "OWN: opens the receipt lease file",
    ),
    (
        "cli/web/claude_questions.rs",
        "*",
        "ONE-SHOT: Claude's question hook, a process of its own",
    ),
    (
        "git_plumbing.rs",
        "*",
        "ONE-SHOT: git mutations, made by commands; the TUI's plan actions reach them only through queue and plan helpers that ask",
    ),
    (
        "inbox.rs",
        "migrate",
        "ONE-SHOT: moves a legacy inbox aside once, at the first read",
    ),
    ("post_rewrite.rs", "*", "ONE-SHOT: the post-rewrite hook"),
    (
        "priority_dir.rs",
        "*",
        "OWN: a probe file made and removed in one call",
    ),
    (
        "proc_identity/exit_watch.rs",
        "open_to_watch",
        "OWN: opens a directory or file to watch, which writes nothing",
    ),
    (
        "state_cache.rs",
        "*",
        "a cache derived from git, named by its format version: another build never reads this one's entries",
    ),
];

#[derive(Default)]
struct Fns {
    /// (name, first line, last line), innermost last.
    found: Vec<(String, usize, usize)>,
}

fn test_only(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(is_cfg_test)
}

impl<'ast> Visit<'ast> for Fns {
    fn visit_item_mod(&mut self, m: &'ast syn::ItemMod) {
        if !test_only(&m.attrs) {
            syn::visit::visit_item_mod(self, m);
        }
    }

    fn visit_item_impl(&mut self, i: &'ast syn::ItemImpl) {
        if !test_only(&i.attrs) {
            syn::visit::visit_item_impl(self, i);
        }
    }

    fn visit_item_fn(&mut self, f: &'ast syn::ItemFn) {
        if test_only(&f.attrs) {
            return;
        }
        use syn::spanned::Spanned;
        let span = f.span();
        self.found
            .push((f.sig.ident.to_string(), span.start().line, span.end().line));
        syn::visit::visit_item_fn(self, f);
    }

    fn visit_impl_item_fn(&mut self, f: &'ast syn::ImplItemFn) {
        if test_only(&f.attrs) {
            return;
        }
        use syn::spanned::Spanned;
        let span = f.span();
        self.found
            .push((f.sig.ident.to_string(), span.start().line, span.end().line));
        syn::visit::visit_impl_item_fn(self, f);
    }

    fn visit_trait_item_fn(&mut self, f: &'ast syn::TraitItemFn) {
        use syn::spanned::Spanned;
        let span = f.span();
        self.found
            .push((f.sig.ident.to_string(), span.start().line, span.end().line));
        syn::visit::visit_trait_item_fn(self, f);
    }
}

fn code(line: &str) -> &str {
    line.split("//").next().unwrap_or("")
}

/// The production functions of `src` that write without asking.
fn unasked(src: &str) -> Vec<String> {
    let file = match syn::parse_file(src) {
        Ok(file) => file,
        Err(e) => return vec![format!("<does not parse: {e}>")],
    };
    let mut fns = Fns::default();
    fns.visit_file(&file);
    let lines: Vec<&str> = src.lines().collect();
    let mut out = Vec::new();
    for (i, (name, first, last)) in fns.found.iter().enumerate() {
        // A function nested in this one answers for its own lines.
        let inner: Vec<(usize, usize)> = fns
            .found
            .iter()
            .enumerate()
            .filter(|(j, (_, f, l))| *j != i && f >= first && l <= last && (f, l) != (first, last))
            .map(|(_, (_, f, l))| (*f, *l))
            .collect();
        let own = |n: usize| !inner.iter().any(|(f, l)| n >= *f && n <= *l);
        let body = || {
            (*first..=*last)
                .filter(|n| own(*n))
                .map(|n| code(lines[n - 1]))
        };
        let writes = body().any(|line| WRITES.iter().any(|w| line.contains(w)));
        let asks = (*first..=*last)
            .map(|n| code(lines[n - 1]))
            .any(|line| ASKS.iter().any(|a| line.contains(a)));
        if writes && !asks && !out.contains(name) {
            out.push(name.clone());
        }
    }
    out
}

#[test]
fn every_writer_of_a_file_asks_the_authority_or_is_declared() {
    let root = crates_dir().join("cli/src");
    let mut files = Vec::new();
    rs_files(&root, &mut files);
    let production = Production::of(&files);
    let mut undeclared = Vec::new();
    let mut used = vec![false; UNGATED.len()];
    for f in files.iter().filter(|f| !production.is_test_only(f)) {
        let rel = f
            .strip_prefix(&root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        for name in unasked(&std::fs::read_to_string(f).unwrap()) {
            let declared = UNGATED
                .iter()
                .position(|(path, func, _)| *path == rel && (*func == "*" || *func == name));
            match declared {
                Some(at) => used[at] = true,
                None => undeclared.push(format!("{rel}: fn {name}")),
            }
        }
    }
    assert!(
        undeclared.is_empty(),
        "{} function(s) change a file without going through the upgrade authority. Begin \
         each with `let _permit = crate::upgrade::writing()?;`, or take the `&Permit` its \
         caller holds if it settles a delivery; declare it in UNGATED only if a stale \
         process may rightly write it:\n  {}",
        undeclared.len(),
        undeclared.join("\n  ")
    );
    let unused: Vec<String> = UNGATED
        .iter()
        .zip(&used)
        .filter(|(_, used)| !**used)
        .map(|((path, func, _), _)| format!("{path}: {func}"))
        .collect();
    assert!(
        unused.is_empty(),
        "UNGATED declares writers that no longer write unasked; remove them:\n  {}",
        unused.join("\n  ")
    );
}

#[test]
fn the_scan_sees_a_write_and_what_excuses_it() {
    let bare = "fn save(p: &std::path::Path) { std::fs::write(p, b\"x\").unwrap(); }\n";
    assert_eq!(unasked(bare), vec!["save"]);
    for asked in [
        "fn save(p: &std::path::Path) -> std::io::Result<()> {\n    let _permit = \
         crate::upgrade::writing()?;\n    std::fs::write(p, b\"x\")\n}\n",
        "fn save(p: &std::path::Path, _permit: &crate::upgrade::Permit) {\n    \
         std::fs::write(p, b\"x\").unwrap();\n}\n",
        "impl S {\n    fn save(&self) -> std::io::Result<()> {\n        let _p = \
         self.guard.permit()?;\n        std::fs::rename(\"a\", \"b\")\n    }\n}\n",
    ] {
        assert_eq!(unasked(asked), Vec::<String>::new(), "{asked}");
    }
    let method =
        "impl S {\n    fn save(&self) {\n        let _ = std::fs::File::create(\"a\");\n    }\n}\n";
    assert_eq!(unasked(method), vec!["save"]);
    let commented = "fn save() {\n    // crate::upgrade::writing()\n    let _ = std::fs::remove_file(\"a\");\n}\n";
    assert_eq!(unasked(commented), vec!["save"], "a comment asks nobody");
    let tested =
        "#[cfg(test)]\nmod tests {\n    fn save() { std::fs::write(\"a\", \"b\").unwrap(); }\n}\n";
    assert_eq!(unasked(tested), Vec::<String>::new(), "test code is exempt");
    let nested = "fn outer() -> std::io::Result<()> {\n    let _permit = crate::upgrade::writing()?;\n    \
                  fn inner() { std::fs::write(\"a\", \"b\").unwrap(); }\n    inner();\n    Ok(())\n}\n";
    assert_eq!(
        unasked(nested),
        vec!["inner"],
        "a nested function asks for itself"
    );
}

/// The seams through which a long-lived process changes something
/// outside itself without writing a file: typing into a pane, a native
/// harness call, a recovery dispatch, an interrupt, driving
/// panes, a git mutation. As (path under `crates/cli/src/`, function).
///
/// Named, not discovered, and that is the limit of this half of the
/// gate: an action has no spelling of its own. A subprocess or a socket
/// call that changes an agent reads exactly like one that only looks, so
/// no scan can tell a new action from a new read. What a scan can do is
/// keep each known seam asking, and fail when one is renamed away. New
/// routes are kept INTO these seams by the gates that already exist:
/// `zellij_ownership_boundary` (only the reconcile worker drives panes),
/// `process_boundary` (every spawn through `proc_run`) and `git_boundary`
/// (every git mutation through `git_plumbing`).
const ACTION_SEAMS: &[(&str, &str)] = &[
    ("cli/web/mod.rs", "say_with"),
    ("cli/pane_input.rs", "stop_pane"),
    ("cli/pane_input.rs", "nudge"),
    ("cli/pane_input.rs", "submit_async"),
    ("cli/pane_input.rs", "wake_for_events_async"),
    ("cli/pane_input.rs", "say_to_pane_for_tool"),
    ("cli/codex_chat.rs", "send"),
    ("cli/codex_interrupt.rs", "try_wake"),
    ("cli/codex_interrupt.rs", "start_if_failed"),
    ("cli/event_interrupt.rs", "dispatch"),
    ("cli/monitor.rs", "typed_send"),
    ("cli/monitor.rs", "act_on_recovery"),
    ("cli/status_tui/zellij.rs", "handle_with"),
    ("git_plumbing.rs", "output"),
    ("git_plumbing.rs", "open"),
    ("git_plumbing.rs", "on_the_terminal"),
];

#[test]
fn every_action_seam_asks_the_authority() {
    let root = crates_dir().join("cli/src");
    let mut missing = Vec::new();
    for (path, name) in ACTION_SEAMS {
        let src = std::fs::read_to_string(root.join(path)).unwrap();
        let mut fns = Fns::default();
        fns.visit_file(&syn::parse_file(&src).unwrap());
        let lines: Vec<&str> = src.lines().collect();
        let asks =
            fns.found
                .iter()
                .filter(|(found, _, _)| found == name)
                .any(|(_, first, last)| {
                    (*first..=*last).any(|n| code(lines[n - 1]).contains("upgrade::writing()"))
                });
        if !asks {
            missing.push(format!("{path}: fn {name}"));
        }
    }
    assert!(
        missing.is_empty(),
        "these act on something outside the process and no longer ask the upgrade \
         authority first (or were renamed without this list):\n  {}",
        missing.join("\n  ")
    );
}
