# opencode-agent-tool
# opencode as a clank agent tool

## Why

opencode (brew `anomalyco/tap/opencode`, v1.18.x) is a provider-
agnostic agentic CLI: adding it as a clank tool gets every provider it
speaks — the motivating one being Moonshot/Kimi, which codex just
dropped (its Responses-only wire API broke `wire_api = "chat"`
providers). A `kimi` clank agent becomes
`tool: opencode` + launch args `--model moonshot/kimi-k3`.

## Scouted facts (v1.18.11)

- Sessions are first-class: `opencode [project]` TUI, `opencode run`
  headless, `-s/--session <id>` and `--continue` resume, `opencode
  session list`, export/import; session db under opencode's data dir.
- NO session-id env var is exported to shell tools (nothing like
  CLAUDE_CODE_SESSION_ID) — the binding gap.
- A JS plugin system (`opencode plugin`, config-registered) with the
  events we need: `session.created`, `session.idle`,
  `tool.execute.before/after`.
- opencode natively loads Claude Code skills (there's an
  `OPENCODE_DISABLE_CLAUDE_CODE_SKILLS` kill-switch), so clank's
  skills in `~/.claude/skills` are expected to load AS-IS.

## Milestones

### M0 — prerequisite: the minimal identity + hook slice the spike needs

Everything M1's probes invoke must already parse and route (codex
eac7103/f7a886d) — landed with unit tests before any live probing, so
the no-go decision measures OPENCODE's compatibility, never a known
clank gap:

- `SessionId` accepts opencode's `ses_<payload>` shape (underscore
  admitted deliberately; parser + binding-persistence round-trip
  tests).
- `Tool::OpenCode`: config serialization + CLI parsing (`--tool
  opencode` everywhere ToolArg reaches).
- Exact `OPENCODE_SESSION_ID` env detection in the identity resolver,
  mapped to `Tool::OpenCode`, and `clank as` binding persistence.
- Enough `clank stop-hook --tool opencode` routing to invoke the
  candidate work loop from the plugin.

Launch profiles, full start/resume composition, setup/doctor, and
docs stay in M2/M3 behind the spike gate.

### M1 — SPIKE: the clank opencode plugin (binding + work loop)

A small bundled JS plugin, installed by `clank setup`, probed live
before anything else builds on it:

- **Binding**: expose the session id to shell tools — preferred
  mechanism is opencode's documented `shell.env` hook (purpose-built
  for adding env to shell executions), with `tool.execute.before`
  only as fallback. NOTE (codex 2ad92d4): `clank as` does NOT work
  unchanged — opencode ids are `ses_<payload>` and clank's SessionId
  parser rejects underscores; M0 extends the id shape. The spike must
  prove the EXACT generated id end to end:
  `shell.env` → `clank as` → stored binding → `opencode -s` resume.
  `shell.env`'s sessionID is OPTIONAL — user PTYs invoke the same
  hook with no session context — so the plugin injects
  OPENCODE_SESSION_ID only when the hook call supplies the exact
  sessionID, and NEVER falls back to cwd or global state (a guessed
  binding is worse than none).
- **Work loop**: `session.idle` is the Stop-hook analog. The spike
  proves the plugin can, on idle, invoke `clank stop-hook --tool
  opencode` style logic and INJECT a continuation prompt into the
  session (opencode's server API allows sending messages). If
  injection is impossible, fall back to the grok model: passive tool,
  skill-carried arming of a background `clank wait` (spike verifies
  opencode's bash tool supports long-lived background processes).
- The spike's findings gate M2's shape and are recorded in the plan
  file as a revision.
- **NO-GO RULE (lloyd, explicit): if the spike shows opencode does
  not fit clank's model cleanly** — no reliable session binding, no
  workable work loop (neither plugin injection nor background waits),
  or only hack-tier workarounds — **do NOT continue to M2.** Run
  `clank block create opencode-agent-tool -m "<findings>"` and stop
  for the human's decision. A messy integration is worse than none;
  forcing the tool in is explicitly rejected.

### M1 SPIKE FINDINGS (recorded 2026-08-03 — live against opencode 1.18.11, free-tier model, zero cost)

ALL GREEN — the no-go rule is NOT triggered; opencode fits the model
without hacks:

- **Binding**: the `shell.env` hook receives exact `{cwd, sessionID,
  callID}` per tool call with a mutable `env` output. A ~20-line
  probe plugin injected `OPENCODE_SESSION_ID`; a live in-session
  `clank as kimi` (M0 binary) bound and persisted
  `tool: opencode, id: ses_039d60658ffe0RPgue3noZ0Qqf`. Exact-session-
  only injection is trivial (inject iff `input.sessionID`).
- **Resume**: `opencode run -s <id>` continued the session
  (RESUMED-OK); the TUI parses `--session <id>` (validated a bogus id
  with a proper error) — M0's restore argv is correct as written.
- **Work loop**: `session.idle` fires with `properties.sessionID`;
  `ctx.client.session.prompt({path:{id}, body:{parts:[{type:"text",
  …}]}})` injected a continuation the model then PROCESSED (both the
  ping and the reply are in the exported transcript). The loop is
  codex-shaped: on idle, the plugin runs `clank stop-hook --tool
  opencode` and injects non-empty continuations — no background-wait
  arming needed.
- **The REAL command path, three states, live** (codex 8000d6e —
  the opencode hook arm now routes through compute_wait_outcome like
  codex): no work + wait timeout → EMPTY stdout, exit 0 (nothing to
  inject, session stays quiescent — an arming-nudge relay would loop
  forever and is gone); staged work → exactly ONE rendered work
  prompt ("Clank wait returned work for `kimi` (master)… Act on
  these items now."); work handled → empty again. The plugin's whole
  injection rule: inject iff stdout is non-empty.
- **Leak case observed live**: opencode launched from a claude shell
  inherits CLAUDE_CODE_SESSION_ID; the pre-M0 installed clank
  misbound the claude session, and M0's ConflictingSessions selector
  errors loudly on exactly that state. The production plugin/launch
  docs should note scrubbing inherited session vars.
- opencode natively exports `OPENCODE=1` and `OPENCODE_PID` but no
  session id — the plugin remains the binding carrier, as designed.
- Free-tier models (`opencode/…-free`) make CI-free live probing
  cheap for future manual verification.

### M2 — core tool support

- `Tool::OpenCode` (config `"tool": "opencode"`) through the roster,
  `agent add --tool opencode`, launch profiles (`launch.args` carries
  `--model provider/model` — moonshot documented as the example),
  `clank agent start` fresh + resume (`-s <bound-id>`), `--print`
  parity, session binding env detection per M1.
- Work-loop wiring per M1's outcome (RESOLVED: plugin-driven,
  codex-shaped; the hook routing landed with the spike): the
  production plugin invokes `clank stop-hook --tool opencode` on
  `session.idle` and injects non-empty stdout via
  `client.session.prompt`; empty stdout = quiescent. The grok-style
  auto-prompt interim from M0 is replaced by hook-driven
  orchestration. Plugin loop discipline (codex 8000d6e): AT MOST ONE
  in-flight stop-hook wait per session (an idle during a pending wait
  is ignored), and a completed wait's continuation is DISCARDED
  unless the session is still idle at injection time — user activity
  between idle and result must not get a stale prompt stomped into
  the conversation.
- Env hygiene (codex 8000d6e, observed live): `clank agent start`
  launches of opencode AND the plugin's own shell path scrub foreign
  session vars — CLAUDE_CODE_SESSION_ID, CODEX_THREAD_ID, GROK_AGENT,
  and stale OPENCODE_SESSION_ID — with deterministic tests; a
  clank-started opencode session beneath another agent must bind,
  not ConflictingSessions-error.

### M2 FINDINGS (recorded 2026-08-03 — production plugin verified live end to end)

The full production loop was proven against `opencode serve` + an
attached run on a free-tier model: `session.idle` → plugin runs
`clank stop-hook --tool opencode` → the rendered work prompt
("Clank wait returned work for `kimi` (master)… Act on these items
now.") appears as a user message in the exported transcript, exactly
once. Wire details the probes surfaced:

- **The hook's stdin needs the FULL HookInput shape** —
  `stop_hook_active` is required, and a partial shape is swallowed
  SILENTLY (parse errors exit 0 with empty stdout, indistinguishable
  from no-work). The plugin pins the complete shape; M3's doctor
  should consider a wire self-check since this failure mode is
  invisible by design.
- **Staleness must count NEW USER MESSAGES only.** opencode emits
  housekeeping right AFTER `session.idle` — `session.updated`,
  `session.diff`, and a final assistant-role `message.updated` — so
  counting all session/message events marks EVERY wait stale
  (observed live: continuations were silently discarded).
- **`opencode run` one-shots cannot host the loop**: the process
  exits right after the reply, killing the plugin's pending wait.
  Fine — production panes run the long-lived TUI/server. Probes must
  attach to `opencode serve`.
- **Plugins load from the GLOBAL config dir only**
  (`~/.config/opencode/plugin/`); a project-local `.opencode/plugin/`
  is NOT loaded (1.18.x). M3's setup installs globally.
- `opencode export` through a pipe truncates at ~64KB; export to a
  file when verifying.

### M3 — setup, doctor, docs

- `clank setup`: install/register the plugin (global opencode config
  dir), verify skill visibility (native Claude Code skill loading;
  only add opencode-native skill copies if the spike shows the native
  path misses them), stop-hook/auto-mode config parity.
- `clank doctor`: opencode section (binary present, plugin installed
  and version-matched, skills visible, binding env round-trip).
- README + master-skill mention; `clank agent add kimi --tool
  opencode` example with the moonshot launch profile.

## Out of scope

- Provider/auth management (users run `opencode providers`).
- ACP/serve integrations beyond what the plugin needs.
- Console/zellij changes: agent panes already run `clank agent start
  <label>`, which M2 makes work.

## Acceptance

- Spike findings recorded; the chosen binding + work-loop mechanism
  demonstrated against a real opencode session (manual probe, not CI).
- A roster with an opencode agent: add, promote-able, `clank agent
  start <label> --print` shows the right argv (model args included),
  start resumes the bound session, `clank as` binds from inside.
- The work loop turns: an opencode reviewer receives work via wait
  delivery and writes feedback through a full review round in a live
  probe repo.
- setup + doctor cover the new tool with the same drift checks as the
  other tools; all existing tool paths untouched (tests pin the tool
  enum round-trip and launch argv shapes; no binary-spawning tests).
