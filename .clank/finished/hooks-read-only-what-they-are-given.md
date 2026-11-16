# hooks-read-only-what-they-are-given

Invariants: I4, I6.

## Why

The rule "hooks must not read configuration owned by other components" is enforced by `hook_is_a_messenger`, a substring list over seven files. A reviewer reintroduced the exact `outage_notified` config read that broke CI into the Grok hook through `load_all_agent_configs`, and through `wait::blocked`. All four gate tests still passed.

A substring list over the hook files cannot see what the functions they call do. The rule has to be enforced over what hooks *reach*.

Two cases that looked like the same problem are not:

- `agent_store::identity::tool` is a deliberately narrow, binding-only reader for adapters: "Bootstrap needs only the roster's declared tool, never role or launch policy". Replacing it with the full roster parser would make hooks read policy. It stays. What is missing is a guarantee that the two parsers agree.
- `EventLog::open` reads config only through `watch_store::log_location`, to resolve `watch:` string identities to paths. That belongs to `a-watch-is-a-typed-producer`, which removes the string identities. It moves there.

## Build

1. **A reachability gate.** It *adds* transitive coverage to `hook_is_a_messenger` and does not replace its checks.
   - **Roots.** The walk starts at the real process entries:
     - `stop_hook::run`, which dispatches Stop, SessionStart, PostCompact and subagent events;
     - `turn_child::run`;
     - `grok_hook::hook`;
     - `question_hook::run`;
     - `recovery_hook`'s and `hook_handoff`'s entries.

     The gate first checks that each root exists by path, so a rename fails instead of silently guarding nothing.
   - **The graph.** `syn` parses `crates/cli/src` into a call graph of free and associated functions by path, and method calls by name. The graph is over-approximated, so the gate fails closed. Syntax it cannot follow is rejected, never skipped: a function passed as a value or callback, a `use` alias, or a call inside a macro other than the formatting and assert family. The gate fails naming the site, unless that site is listed in an explicit, reasoned allowlist of such forms.
   - **What the walk may not reach.** Three kinds of check apply to every function reachable from a root, not only to the hook files:
     - **today's per-line rules** in `hook_is_a_messenger`: decision-state file names, writes, local policy decisions;
     - **a deny list of policy and state reader functions:** `load_agent_config`, `load_all_agent_configs`, `repo_config::read`, `resolve_effective_auto_mode`, block, question, recovery, ledger and turn-custody state readers;
     - **direct file access to decision state:** any reachable `fs::read*`, `fs::write`, `OpenOptions`, `File::open`/`create` or `persist` whose path names a decision-state file (`recovery*.json`, `question.json`, `turn.json`, `state.json`, ledger files, block files).
   - **Narrow exceptions.** An allowed exception (the binding through `identity::*`, the transcript, process identity, upgrade state) exempts one named function from the reader deny list. The walk still continues into it, so a policy read added inside an allowed helper is still caught.
   - **Unchanged:** the OpenCode plugin boundary check (`opencode_plugin_carries_native_events_without_config_access`).
   - **Failures** name the path from the root to the violation.
2. **Typed hook input.** Each hook entry builds one value from its stdin and the binding it routes by, and passes only that down. Where this is already true, the gate proves it, and the step is a check, not a rewrite.
3. **The two roster readers agree.** A test runs `identity::tool` and the full roster parser over every roster fixture and the format fixtures, and requires the same tool for every label. Both read one shared schema constant for the `agents.<label>.tool` field, so a rename breaks both.
4. **Registry:** I4's `Tests` names the new gate test, and its `Plans` drops this plan.

## Acceptance

Each probe fails the gate and names its path:

- A call to `load_all_agent_configs` added to `grok_hook.rs`.
- A call to `wait::blocked` added to a hook.
- A config read added inside a helper a hook already calls.
- A config read added in each of:
  - SessionStart handling;
  - subagent handling;
  - `turn_child::run`'s startup before `carry`.
- A helper reachable from a hook that does `fs::read` on `recovery.json`.
- A helper that does `fs::write` on `question.json`.
- A policy read added inside an allowed binding helper.
- A hook passing a denied function as a callback.

Also:

- Renaming a root fails the gate.
- Removing an exception fails the gate on that path.
- The existing per-line and OpenCode checks still pass and still catch their own probes.
- The roster agreement test fails when one parser is changed to read the tool from a different field.
