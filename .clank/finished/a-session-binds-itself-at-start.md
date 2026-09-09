# a-session-binds-itself-at-start

> Something happened with codex where I think I did /clear on it and it
> got a new session id but clank didn't see this, so it stopped
> tracking the real session. If there's a session-start hook, couldn't
> we avoid the `clank as claude` thing and just do that in the hook?
> That would be cleaner. — lloyd

Yes on both counts, and they are one change.

## What breaks today

Clank knows an agent BY SESSION ID. `clank as codex` writes the id
into `.clank/agents/codex/config.json`; every hook call resolves "who
is this" by looking the id on stdin up in those files
(`resolve_identity_for_hook`). Nothing else links a running process to
its label.

`/clear` (and `/new`) hands the running process a fresh session id —
claude and codex alike — and nothing rebinds. From then on the Stop
hook fails resolution with "no agent set up for codex session …", as
a Diagnostic (exit 0, one stderr line nobody sees), so turn-ends stop
parking a wait and the agent silently stops being driven. The stale
binding also still names the old thread, so the next `clank agent
start` / `clank open` resumes the OLD session. That is the report.

## The hook exists, for both tools

Claude Code's `SessionStart` hook carries `session_id`, `cwd` and
`source ∈ {startup, resume, clear, compact}`. Codex 0.153's does too —
read from the schema embedded in its binary: the same fields, the same
four sources, and the same `hookSpecificOutput.additionalContext`
output shape. Clank already installs a SessionStart hook for claude
(asyncrewake mode; `clank stop-hook --tool claude --session-start`)
and uses it to mint the wait generation and surface pending work; it
resolves identity by the new id, so on `clear` it returns quietly. For
codex clank installs no SessionStart hook at all.

## The model

> A session clank launched binds itself when it starts, and rebinds
> whenever its id changes. `clank as` is for a session clank did not
> launch.

The link the payload lacks — WHICH agent this process is — is put
into the process by the thing that knows: `clank agent start <label>`
exports `CLANK_AGENT=<label>` into the tool it execs. The launch
already scrubs every inherited identity var (`SESSION_IDENTITY_VARS`,
`CLANK_AGENT` among them) before exec, so this is the process tree's
OWN label set after the scrub, never a parent's leaking through; a
tool nested under it scrubs again. Hooks inherit the tool's
environment, so every hook the tool spawns knows the label whatever
the session id says — and `resolve_identity_for_hook` already lets an
explicit `CLANK_AGENT` win, so the Stop hook keeps resolving across a
`clear` even before the rebind lands.

The SessionStart handler (`run_session_start`) then, when
`CLANK_AGENT` is set:

1. **binds** `label → (tool, session_id)` via `bind_session_to_agent`
   — on `startup` that is the first bind (the id exists for the first
   time), on `clear` the rebind that fixes the report, on `resume` and
   `compact` an idempotent re-assertion. `bind_session_to_agent`
   already mints the wait generation as part of the claim and clears
   the id off any other label that held it, so the stale-waiter
   revocation the handler does today is subsumed, not duplicated;
2. carries on exactly as today: the pending-work greeting via
   `additionalContext`, fail-open everywhere.

Without `CLANK_AGENT` the handler does what it does today — resolve by
session id, and quietly nothing when that fails. A session the user
started by hand is bound by `clank as`, as now.

**Claude's SessionStart hook is installed in every mode.** It rode
with the asyncrewake delivery mode alone, and a legacy-mode claude
would have had its bind prompt taken away with nothing writing the
id (codex on acdfb0e). Binding does not depend on how work is
delivered, so the companion is installed beside the Stop hook
whatever mode the probe chose, a downgrade keeps it, and the doctor
wants it — for both tools — independently of the loop-mode check.

**Codex gets the SessionStart hook.** `clank setup` installs
`clank stop-hook --tool codex --session-start` into
`~/.codex/hooks.json` beside the Stop hook, through the same asset
inventory the doctor checks (setup/doctor parity by construction).
The handler's stdin reading and output shape are shared with claude's;
`--tool` is what differs. Whether codex hands its environment to hook
processes is verified at implementation with a hook that echoes
`CLANK_AGENT` — claude documents that it does; codex is expected to
and this is the one assumption the change rests on.

**The bootstrap prompt stops asking for a bind.** Today an unbound
launch seeds `Run \`clank as <label>\` to bind this session.` — the
prompt exists to make the agent run a command AND to end a first turn
so the Stop hook parks. Binding is no longer the agent's to do, but a
first turn still has to end, so the seed becomes the same shape the
resume path already uses (`Session resumed.`): `Session started.`,
name-led for the tools that derive a title from it, with the
SessionStart greeting carrying any pending work. The pinned-string
test moves with it.

`clank as <label>` stays, unchanged, as the manual path; its skill
line becomes "your session binds itself when clank launched it; run
`clank as <label>` only in a session you started by hand".

## What this does NOT do

- opencode and grok: opencode's plugin injects a session id per shell
  and has no SessionStart wire to clank; grok's hooks are passive. Both
  keep the bootstrap bind prompt and `clank as`. The prompt choice is
  per tool, where the seed is already composed per tool.
- Detect a `clear` in a session with no `CLANK_AGENT`: there is
  nothing to detect it against.

## Tests

- `exec_composed`'s environment carries `CLANK_AGENT=<label>` AFTER the
  scrub: a parent `CLANK_AGENT=other` and a parent session var are
  both gone and the launched label is present — asserted on the
  composed command's env, no spawn.
- `run_session_start` with `CLANK_AGENT` set and each `source`: the
  label's config holds `(tool, session_id)` afterwards; on `clear` a
  binding to the OLD id is replaced; a previous holder of the same id
  is cleared; the wait generation moved.
- Without `CLANK_AGENT`: nothing is bound, and an unbound id is the
  quiet no-op it is today.
- `clank setup` writes the codex SessionStart entry; the doctor
  inventory lists it; a legacy `hooks.json` with only the Stop entry
  gains it without disturbing the Stop entry.
- The bootstrap seed for claude and codex no longer mentions
  `clank as`; opencode's and grok's still do.
- The README quickstart test (`the_readme_quickstart_launches_the_agents`)
  follows the README's step 3.

Mutation-checked with production-only edits: the export removed; the
bind skipped on `clear`; the codex entry not installed.

## Out of scope

- Removing `clank as`.
- Any change to how a bound session is resumed or attached.
