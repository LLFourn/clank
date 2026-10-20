# a-changed-tool-starts-the-new-tool
# A changed tool starts the new tool

## Why

Life's claude filed this for Lloyd on 2026-10-20, as clank message 57.

The repro:

1. `browser2` was `{tool: codex, launch.args: [--no-daemon, ...]}`. A Codex session bound to it:
   `.clank/agents/browser2/config.json` holds `session {id 01a156a7-…, tool: codex}`.
2. The roster entry was edited to `{tool: claude, launch: {args: [--chrome]}}`.
3. `clank agent start browser2` ran **codex** with **`--chrome`**:
   `error: unexpected argument --chrome found … Usage: codex`.

`agent start` lets the bound session's tool win over the roster's. That rule is deliberate (codex ff3ca44): a codex session must get codex launch side effects even after the roster says claude. But the launch profile still comes from the roster, which now describes the other tool. So the start mixes one tool's session with another tool's arguments. The workaround was deleting `.clank/agents/browser2` by hand.

## The contract

When the roster's tool differs from the bound session's tool:

- **The configured tool starts.** `agent start` bootstraps the roster's tool with the roster's launch profile, exactly as for an agent with no session: the seed prompt to `clank as <label>`. The new session binds over the old one when it reports in.
- **The old session is not lost.** The old binding is kept, unresumed, until the new session binds. Start says which tool's session it is setting aside, and its id, so the operator can resume that tool by hand.
- **Nothing ever mixes.** A composed launch uses the launch profile only with the tool that profile belongs to.
- **The mismatch is visible:**
  - `agent list` flags it;
  - `status` flags it;
  - `agent launch <label>` and `agent start --print` show that the next start bootstraps the new tool.

## Scope

- `cli/agent.rs`:
  - start's choice between resume and bootstrap: a bound session of another tool is treated as unbound for composition;
  - the note start prints;
  - the preview path.
- `agent list` and `status`: the mismatch flag.
- No change to binding itself, or to resumes when the tools match.

## Acceptance

Through the in-process cores, with no binary spawned:

- **The repro.** A codex binding plus a roster entry for claude with `--chrome`. The composed launch is claude, bootstrapping, with `--chrome`; `codex` is not in it, and neither is the codex session id. The note names the codex session.
- **Matching tools** still resume, with the same composed line as before.
- **The binding is untouched by the start itself**, and the next bind replaces it.
- **The flag shows** in `agent list` and `status` while the tools differ, and is gone once the new session binds.
- **Mutation check:** the old "the bound tool wins" composition must fail the repro test.

## As built

- **`superseded_session(configured, bound)` in `cli/agent.rs` is the one rule:** a bound session whose tool is not the roster's is set aside. Start, `agent list` and `status` all read it.
- **`start_with` resolves the choice first,** before the Grok profile check, holder lookup, setup, trust, environment and prompt.
  - A superseded binding is treated as no binding for composition, so the configured tool bootstraps with its own profile and binds through `CLANK_AGENT`.
  - `fork_intent(repo, label, configured, superseded, print)` decides the fork intent. It is followed only when no binding is superseded and its tool is the configured one. Otherwise it is set aside with a note.
  - On a real start (not `print`) the set-aside intent is retired before setup, so its stale Grok child cannot later gate the fresh session's `clank as` through `grok_launch::check_binding`. A preview retires nothing. A matching intent is followed and kept, as before.
  - The note names the old tool and session id, and says the binding stays until the new session reports in.
  - In print mode `start_with` now returns the composed launch, so tests read the real selection path.
- **`agent list`:** `AgentRow` gains `configured_tool` in the JSON. The tool column reads `codex → claude next start`.
- **`status`:** `AgentAutoRow::superseded()` prints `tool changed: <label> → bound to a <old> session, configured <new>; its next start runs <new> fresh`.
- **Unchanged:** binding itself, and resume and fork when the tools match.
- **Tests:**
  - the Codex-to-Claude repro, for both start and preview: `claude` with `--chrome`, no codex session id, binding as `browser2`, and the agent's files byte-identical before and after;
  - a Grok binding under a codex roster never takes the Grok path;
  - an older codex fork intent under the mismatch is not followed, and a preview leaves it;
  - an unbound agent's fork intent for another tool is set aside;
  - switching back to Grok over a Codex binding, with an old prepared Grok fork intent (codex on e112390): the preview is fresh and keeps the intent; a real start retires it; the real `bind_session_to_agent` with the fresh Grok id then succeeds, and the mismatch is gone;
  - a matching fork intent is followed and kept;
  - matching tools still resume the session;
  - list shows the switch;
  - status says it, and stops once a session of the configured tool is bound.
- **Mutations, each caught:**
  - the bound tool winning;
  - loading the fork intent under a superseded binding;
  - not filtering a fork intent by tool;
  - never retiring a set-aside intent, and retiring one on a preview;
  - start not going through `fork_intent`;
  - list unflagged;
  - status unflagged.
