# grok-first-class
# grok first-class support + research team

Add xAI's Grok CLI ("Grok Build TUI", binary `grok`, currently 0.2.93) as a
first-class clank tool alongside claude/codex, and register a `research`
team: grok master, codex commit reviewer, claude final reviewer. Grok is
the research tool (X search); the team exists to drive research plans
through the normal review gate.

## Research findings (bundled v0.2.93 user guide + local probing)

- **Hooks**: claude-style `hooks.json`. Global `~/.grok/hooks/*.json` is
  ALWAYS trusted (no prompt); project hooks require folder trust
  (`~/.grok/trusted_folders.toml`, granted via `/hooks-trust`). Events
  include `Stop`, but **every event except PreToolUse is passive** —
  stdout ignored, exit codes fail-open. Neither claude's exit-2
  continuation nor codex's block-with-items can drive grok from a Stop
  hook. This is the central integration constraint.
- **Background tasks**: `run_terminal_command` with `background: true`;
  completion "appears in the conversation" as a notification; in-turn
  blocking waits via `get_command_or_subagent_output(task_id,
  timeout_ms)` and `wait_commands_or_subagents`; a scheduler whose
  firings "create a new agent turn".
- **Sessions**: UUIDs under `~/.grok/sessions/<encoded-cwd>/<id>/`;
  `--resume <id>`; fork = `--resume <id> --fork-session --session-id
  <new>` (clank can NAME the forked session); fresh named session =
  `--session-id <uuid> "<prompt>"` (prompt is a positional arg);
  launch profiles via `--agent <name>`.
- **Claude compat**: grok already loads clank's `.claude` allow rules
  (54 rules in `grok inspect`), clank's three claude skills (tagged
  `[claude]`), CLAUDE.md, and the claude Stop hook (which no-ops
  passively under grok). Much of the surface works today by accident;
  the plan makes it deliberate.
- **Hook env**: hook processes get `GROK_SESSION_ID` /
  `GROK_WORKSPACE_ROOT`. Whether the AGENT'S shell commands (the
  `run_terminal_command` subprocess env) see a session id is UNKNOWN —
  probe P2.
- **UNKNOWN (decisive)**: whether a background-task completion
  notification starts a NEW agent turn (claude-style wake) or sits
  until the next prompt — probe P1.

## Milestone 0 — probes (append results to this plan doc before code)

Run a live grok session in a scratch repo and pin:

- **P1 wake semantics**: arm `sleep 5` with `background: true`, end the
  turn. Does completion start a new turn unprompted?
- **P2 shell env**: run `env | grep -i grok` via the terminal tool — is
  a session id visible to shell commands (needed for `clank as` and
  identity resolution)?
- **P3 skill collision**: with `clank-master` present in BOTH
  `~/.grok/skills/` and the claude-compat path, what loads (`grok
  inspect`)? Duplicate, shadow, or dedupe?

A web cross-check of the online docs is in flight; fold anything it
corrects into the findings above.

## Work-loop model (selected by P1)

- **P1 = wakes**: claude-style loop. Arm `clank wait`
  (`background: true`), end turn; the completion wake delivers items.
  No nudge channel exists (passive Stop), so the skill text alone
  carries the arming discipline — acceptable: a dead wait also
  notifies, and the wake loop self-heals.
- **P1 = no wake**: agent-held long-poll. The skill teaches: start
  `clank wait` in the background, then block on
  `get_command_or_subagent_output(task_id, timeout_ms=86400000)`; act
  on items; re-arm. Grok's own docs bless blocking-get over
  sleep-polling. This is codex's model with the agent, not the hook,
  holding the poll.
- Either way: **no grok stop-hook adapter in this plan.** A passive
  hook has nothing actionable to emit today; an observe-only grok hook
  belongs to the future activity-ledger work.

## Implementation milestones

- **M1 core**: `Tool::Grok` in `clank_core` vocab (wire string
  `grok`); exhaustive-match fallout across core+cli; `agent_env`
  identity resolution accepts grok's session env per P2 (if none
  exists, document explicit `clank as <label>` + `--session` as the
  grok bootstrap and make the error message name it).
- **M2 launch/fork**: `agent.rs` composes `grok` launches (profile
  `--agent`, initial prompt positional). REVISED during
  implementation: grok NAMES its own sessions — fresh launches pass no
  `--session-id` and forks are `--resume <from> --fork-session` — and
  the binding lands afterward via `clank as` (newest-session-for-cwd,
  P2), exactly the claude flow. Pre-naming ids would add recording
  machinery no other tool needs, for an id the binding step already
  discovers. Trust: REVISED from trusted_folders.toml surgery (schema
  unverified) to the `--trust` launch flag (real in 0.2.93, hidden
  from --help) appended to every composed grok argv — restore, fork,
  and bootstrap paths — so `--print` previews it honestly.
- **M3 setup**: extend the per-tool skill loop with
  `(Tool::Grok, ".grok")` — clank-master/clank-reviewer composed with a
  `WORK_LOOP_GROK` matching the P1 outcome, plus the pr-review skill.
  Resolve the claude-compat duplicate per P3 (prefer native shadowing;
  else document the collision and the `[compat.claude]` toggle).
- **M4 team**: register a `research` team in `~/.clank/config.json`
  via the typed config: grok master, codex commit reviewer, claude
  final reviewer — such that `clank init --team research` and
  `clank fork --team research` both work.
- **M5 sweep**: one repo-wide grep for tool enumerations (README,
  skills, error strings, docs) so grok appears wherever claude/codex
  are listed and nothing names "the two tools".

## Out of scope

- A grok stop-hook adapter / observe-only hook (activity ledger later).
- ACP (`grok agent stdio`) integration — a powerful future channel for
  clank-driven agents, noted for the record, not built here.
- Any change to claude/codex behavior.

## Acceptance

- `clank agent start <grok-label> --print` shows a correct composed
  launch (binary, profile, session id, prompt) — tested via the
  compose functions, not by spawning grok (no-binary-spawning rule).
- `clank fork` with a grok member emits a ForkSpec whose composed
  launch uses `--resume <from> --fork-session` (grok names the child;
  see the M2 revision); members without sessions fall back to fresh
  launches carrying the orientation prompt.
- Trust: every composed grok launch shape (restore, fork, bootstrap)
  carries `--trust`, so no grok launch can hit the folder-trust
  prompt; the flag never leaks onto claude/codex argv.
- `clank setup` writes grok skills whose work-loop text matches the
  P1-selected model; setup output names the grok surfaces.
- The `research` team registers; `clank init --team research` in a
  scratch repo yields grok master + codex commit + claude final tiers
  in `clank status`.
- Probe results (P1–P3) are recorded in this plan document.
- fmt/clippy at the 18/6 baseline; all tests green; the git-boundary
  and typed-config guards stay green.

## M0 probe results (2026-07-09, grok 0.2.93, ACP stdio + headless probes)

- **P1 — WAKES (claude-style).** ACP probe: turn ended
  (`stopReason: end_turn`), the armed `sleep 8` background task
  completed ~8s later, grok emitted `_x.ai/task_completed`,
  self-enqueued a prompt (`runningPromptId: task-completed-<id>`), and
  a NEW turn began with a `<system-reminder>Background task …
  completed` user message. Work loop = claude-style armed `clank wait`
  (`background: true`), completion wake delivers items.
- **P2 — no session env in shell commands.** Both headless (`-p`) and
  ACP paths: tool subprocess env carries `GROK_AGENT=1` and NO session
  id; `~/.grok/active_sessions.json` stays `[]`. Identity resolution:
  treat `GROK_AGENT=1` as the in-grok marker and resolve the session
  as the newest-mtime session dir under
  `~/.grok/sessions/<urlencoded-cwd>/<uuid>/` (verified live: per-cwd
  URL-encoded group dirs, UUIDv7 session dirs, freshest = running).
  Caveat: two concurrent grok sessions in one repo can misresolve —
  warn, and keep an explicit session override as the escape hatch.
- **P3 — native shadows compat.** With `~/.grok/skills/clank-master/`
  present, `grok inspect` lists ONE `clank-master user` (untagged);
  without it, the `[claude]`-tagged compat copy returns. Setup can
  write grok-native skills that cleanly override the claude ones.
- **Extra:** grok demonstrably runs the claude-settings Stop hook at
  turn end (`hook_execution … global/settings:stop[0]` observed). For
  grok sessions it resolves no identity and grok ignores its output
  (passive) — harmless noise, revisit with the activity ledger.
