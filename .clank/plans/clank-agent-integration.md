# Clank ↔ Claude / Codex Integration

## Summary

Wire clank into Claude Code and Codex CLI so agents call `clank wfw`
without being prompted. Three deliverables:

1. A versioned **clank skill** installed into both agents so they know
   what clank is and how its workflow works.
2. A **Stop hook** for each agent that long-polls `clank wfw` when the
   agent would otherwise end its turn, and resumes the agent with the
   returned work as a continuation prompt.
3. A **`clank setup`** command that writes those files and a **`/clank`**
   slash command (in both agents) that toggles auto-mode without
   editing JSON by hand.

Plus the related smaller pieces: `clank init` adds a master config and
the right gitignores, and per-agent auto-mode lives in
`.clank/agents/<name>/config.json`.

Supersedes the older stubs `agent-automation-hooks.md` and
`codex-stop-hook-wfw-experiment.md`, which conflated several
unrelated ideas (durable background waiters, post-commit notify, etc.)
that this plan deliberately drops.

## Research findings (used by every decision below)

This section is the ground truth the recommendations lean on. Cited
inline rather than re-derived at each option.

### Claude Code

- **Skills**: `~/.claude/skills/<name>/SKILL.md` (user) or
  `.claude/skills/<name>/SKILL.md` (repo). YAML frontmatter + body.
  Auto-discovered; descriptions in context always, body loaded on
  invocation. Slash commands ARE skills — `/clank` works as soon as
  `SKILL.md` exists at the right path.
- **Stop hook**: configured in `~/.claude/settings.json` (or
  `.claude/settings.json`). Fires when claude is about to end a turn.
  Default timeout 10 min; overrideable per-hook. Exit code `2` blocks
  the stop and feeds stderr back to the agent as the continuation
  prompt. Stdin includes `stop_hook_active: bool` — hook MUST honor
  this to avoid infinite continuation (cap is 8 then claude force-stops).
- **No built-in `statusMessage`**. Claude users can't see a "waiting"
  indicator from the hook itself.
- **Plugins** exist (manifest at `.claude-plugin/plugin.json`) but
  install per-user from marketplaces, not per-repo. Overkill for us.

### Codex CLI

- **Skills**: `~/.codex/skills/<name>/SKILL.md` or
  `<repo>/.agents/skills/<name>/SKILL.md`. Same YAML+body shape.
- **AGENTS.md**: codex auto-reads `AGENTS.md` walking from repo root
  to cwd. Per-repo prompt injection without any install step.
- **Stop hook**: `~/.codex/hooks.json` or `~/.codex/config.toml`.
  Stdin shape includes `stop_hook_active`. Continuation contract is
  `{"decision":"block","reason":"..."}` on stdout OR exit 2 + reason
  on stderr. `timeout` in seconds, default 600. `statusMessage` shows
  in the TUI while the hook runs.
- **First-run trust prompt**: any non-managed hook command requires
  user approval the first time and after every edit. `clank setup`
  cannot bypass this; it can only document it.
- **Slash commands**: markdown files under `commands/`. Repo-scope
  path under `.agents/` (also `~/.codex/commands/`). `!`bash`` inline
  exec and `$ARGUMENTS` work.
- **Plugins** exist with their own manifest under `.codex-plugin/` and
  a local marketplace mechanism. Real but heavyweight.

### Both agents

- **`stop_hook_active` is mandatory.** If we forget to short-circuit
  on it, claude force-stops after 8 loops and codex loops forever.
- **Per-tool JSON output shapes differ.** Claude: exit 2 + stderr OR
  `{"ok":false,"reason":...}`. Codex: `{"decision":"block","reason":...}`.
  An adapter command has to format per tool.
- **Per-repo hook config is hostile in both tools** (codex requires
  project trust; claude project hooks are committed and shared).
  User-scope wins for our case.

---

## Decisions

For each: the question, the options, and the recommendation. The
implementation section at the bottom assumes every recommendation is
accepted; if any is rejected, that section needs revisiting.

### D1. Packaging: plugin vs. standalone files

**Question.** Do we ship as a "plugin" in each agent's plugin system,
or write loose files into well-known paths?

**Background — how Stop hooks actually work without a plugin.** Both
agents read hook config from well-known JSON files regardless of
where those files came from. For claude it's
`~/.claude/settings.json`:

```json
{
  "hooks": {
    "Stop": [
      {
        "hooks": [
          { "type": "command",
            "command": "clank stop-hook --tool claude",
            "timeout": 1800 }
        ]
      }
    ]
  }
}
```

For codex it's `~/.codex/hooks.json` with the same shape. A "plugin"
is just a bundle that *contains* a `hooks.json` (plus a manifest, UI
icons, etc.) and gets installed into the agent's plugin directory
where the agent then reads it. Either way, the running hook is the
same JSON entry on disk; the difference is whether the install path
goes through a marketplace UI or is just "clank wrote these files".

- **Option A — Full plugins** (`.claude-plugin/plugin.json` + Codex
  plugin manifest, published to marketplaces or installed locally).
  Pro: standard discovery, plugin UI, easy upgrade story. Con: two
  different manifest formats, marketplace plumbing, plugin-trust
  prompts (codex), plugin updates managed by the agent and not by
  clank — so the clank binary and the installed clank-plugin version
  can drift.
- **Option B — Standalone files** written by `clank setup` directly
  into `~/.claude/skills/clank/`, `~/.codex/skills/clank/`,
  `~/.claude/settings.json` (merge), `~/.codex/hooks.json` (merge).
  Pro: skill text always matches installed clank version; one path
  to maintain; user upgrades clank → next `clank setup` updates the
  integration. Con: not a "real" plugin; we merge into shared settings
  files (tagged-merge handles this — D8).
- **Option C — Hybrid**: real plugin layouts, but installed locally
  via the agent's local-marketplace mechanism (no publishing). Pro:
  appears in plugin UI. Con: most of A's cost without much benefit
  while we're pre-public.

**Recommendation: B.** Clank's identity is "a CLI you install"; the
integration assets are just data the CLI emits. Revisit when there's
a reason to publish (e.g. people installing clank without ever running
`clank setup`).

### D2. The hook command — wrap `wfw` or new verb?

**Question.** What does the Stop hook line in settings.json actually
invoke?

- **Option A — `clank wfw --stop-hook --tool <claude|codex>`**: stuff
  the adapter behavior into wfw's existing flag space.
- **Option B — New verb `clank stop-hook --tool <claude|codex>`**: a
  dedicated agent-facing verb that internally calls the same
  projection wfw does, then formats per-tool output.
- **Option C — Per-agent shell script** that calls `clank wfw --json`
  and adapts the output in bash/jq.

**Recommendation: B.** `wfw` is the user-facing primitive; the Stop
hook is an adapter with extra responsibilities (read stop_hook_active,
read agent config, format per-tool decision JSON, handle "auto off"
silently). Keeping them separate keeps each command's contract clean.
Option C is fragile across platforms and harder to test.

### D3. Auto-mode config location

**Question.** Where does the on/off bit and per-agent settings live?

- **Option A — Per-repo per-agent**: `.clank/agents/<name>/config.json`
  (user's stated preference). Tracks role + auto-mode-on + timeout for
  that agent in this repo.
- **Option B — Per-user**: `~/.clank/agents/<name>.json`. Same agent
  config applies in every repo.
- **Option C — Both, with per-repo overriding per-user**.

**Recommendation: A (with optional fallback to user-wide later).**
Auto-mode is genuinely a repo-scoped concern: I might want auto on
in clank-the-repo but off in some unrelated repo where I happen to
have a clank dir. Per-repo also keeps config next to the agent's
feedback dir which is already where the agent lives. No user-wide
fallback in v1 — add it if a user complains they want one.

### D4. Agent identity — "who am I in this repo?"

**Question.** The hook fires as a subprocess of the agent, gets a
JSON blob on stdin (`session_id`, `cwd`, `transcript_path`, etc.) —
nothing in there says "I'm alice." How does the hook learn the
agent's clank label?

The user's framing crystallized this: the agent (via `/clank`) is
the only entity that can *declare* its identity; the hook needs to
*read* where the declaration was stored. The right storage key
needs to be (a) known to `/clank` at declare time and (b) known to
the hook at read time.

- **Option A — Tool name as default + per-repo override file**
  (`.clank/agents/_identity.json` mapping `{ "claude": "alice" }`).
  Cwd-scoped; one label per (tool, repo). Can't distinguish two
  claude sessions in the same repo.
- **Option B — Session-keyed cache** at
  `.clank/cache/sessions.json` mapping
  `{ "<session_id>": { "label": "alice", "tool": "claude" } }`.
  `/clank as alice` writes the entry keyed by THIS session; hook
  reads same id from stdin. True per-session identity.
- **Option B' — Transcript-as-cache**: parse `transcript_path` JSONL
  backwards for the most recent `clank as <name>` Bash invocation.
  No separate cache file; transcript IS the source of truth.
  Fragile (depends on agent-internal JSONL format).
- **Option C — Env var per shell** (`CLANK_AGENT=alice claude`).
  Requires shell discipline; no first-class flow.
- **Option D — A + B with precedence** for users who care about
  multi-agent-per-repo.

**Recommendation: B.** Confirmed by E2 for both agents:
- claude: `CLAUDE_CODE_SESSION_ID` (UUID v4) — same value the hook
  receives as `session_id` in stdin (formats match; verify literally
  at impl time).
- codex: `CODEX_THREAD_ID` (ULID) — codex calls it "thread" not
  "session", but it's the same role: identifies this agent
  instance, persists for the lifetime of the session, and matches
  the `session_id` field codex sends in hook stdin.

`/clank as alice` (or `clank as alice` from the agent's Bash) reads
the appropriate env var, writes `.clank/cache/sessions.json[<id>]
= { label, tool }`. Hook reads same id from stdin, looks up.

**Bootstrap UX:**
- Fresh install: no session entry yet → hook falls back to tool
  name as label → `.clank/agents/claude/config.json` (auto-created
  empty if missing). Works immediately, no `/clank` ceremony.
- User wants custom name: `/clank as alice` → writes session
  entry → from now on this session is "alice".
- Cleanup: prune session entries older than 7 days on each `clank`
  invocation. Cheap, prevents unbounded growth.

**Tool detection caveat.** Env vars that DO reliably indicate the
running tool:
- claude: `CLAUDECODE=1`
- codex: `CODEX_MANAGED_BY_NPM=1` and/or `CODEX_THREAD_ID` set

Env vars that DO NOT reliably indicate the running tool:
- `AI_AGENT` — leaks across processes. Observed:
  `AI_AGENT=claude-code/...` appeared in a codex session whose
  terminal had been launched from a parent claude shell.

The hook command line still passes `--tool claude|codex` explicitly
from `clank setup`, so clank doesn't need env-based tool detection
at all. The env vars matter only for the session_id lookup, where
they ARE reliable (set by the tool itself, per-process, no leakage
risk from `CLAUDE_CODE_SESSION_ID` / `CODEX_THREAD_ID`).

### D5. Master agent config — separate file or just another agent?

**Question.** Does master need its own file, or is it just an agent
with `"role": "master"` in `.clank/agents/<name>/config.json`?

- **Option A — `.clank/master.json` exists, holds master-specific
  knobs**.
- **Option B — Master is a role flag** in
  `.clank/agents/<name>/config.json`. No new file.
- **Option C — Both**.

**Recommendation: B. Confirmed by user.** Every agent has a label and
lives in `.clank/agents/<name>/`. The agent's `config.json` carries
its current `role` (`master` or `reviewers`). Switching roles is
"edit one field". No `master.json`.

A consequence worth flagging: with role in the config, an agent can
flip role between turns by editing its own config. That's a feature
(a single agent can plan-and-then-review without two install paths)
but it means `clank auto` needs a way to set the role too — `clank
auto on --role master`.

### D6. The `/clank` slash command — scope and shape

**Question.** What does `/clank` do, and how is it invoked?

**Goal: discoverability.** User wants `/clank` to act like a picker
(`/model`-style) rather than a "memorize CLI args" command.

**Research finding (rules out native TUI path).** In both claude and
codex, slash-command shell execution runs in a PTY-captured channel
that streams stdout back as prompt context — stdin is not connected
to the user's tty. No frontmatter flag, no plugin manifest field, no
hook surface, no MCP path hands off the terminal to a subprocess.
The only tty-handoff codepath in codex is hard-wired to
`$VISUAL`/`$EDITOR` for the external-editor feature; not exposed.
Claude has open feature requests for "interactive tty mode" but
nothing shipped. **So `/clank` cannot launch `clank menu` (or any
ratatui-style picker) and let it own the terminal.** Verified
against both agents' binaries and bundled plugins; no plugin in
either ecosystem does this today.

What `/clank` CAN do from a slash command: print text the agent
reads, optionally suggest the agent calls a structured-question
tool (claude `AskUserQuestion`; codex's `elicitation_request` /
MCP analog). That's an LLM-mediated picker — slower, but works.

- **Option A — Passthrough only**: `/clank auto on` maps to `clank
  auto on`. No bare `/clank` menu. Discoverability: nil.
- **Option B — Model-mediated picker**: skill body grounds the agent
  in (current state, available options, command to apply each) and
  asks the agent to use its structured-question tool
  (`AskUserQuestion` in claude; `elicitation_request` / MCP-elicit
  in codex). Slow (LLM turn before the picker shows), but works
  today and gives a real option-picker UI.
- **Option C — Signpost to standalone `clank menu`**: bare `/clank`
  prints current state plus a one-liner: "for an interactive picker,
  run `clank menu` in another shell." No model picker, just a
  pointer. Cheap, honest, but adds friction.
- **Option D — B + C**: model-mediated picker INSIDE the agent,
  standalone `clank menu` TUI for users who want a native picker.
  `/clank` does B; doc string mentions C exists.

**Recommendation: D.** Ship the model-mediated picker (B) as the
in-agent default so `/clank` actually does something useful without
a context switch; ship `clank menu` as a separate ratatui binary
verb for users who want the real thing in their own shell. Cost
of `clank menu` is one new ratatui dep + a config screen — small.

**Invocation shape (per user):**
- `/clank` — bare: show current state and one-liner "for an
  interactive picker, run `/clank config`". No model picker for
  the common "what's set?" check.
- `/clank config` — invoke the model-mediated picker. Cost of an
  LLM turn is OK because user explicitly opted into configuring.
- `/clank <verb> [args]` — passthrough (e.g. `/clank auto on`,
  `/clank as alice`). Verb resolves to a `clank` CLI command.

Skill body sketch:

```markdown
---
name: clank
description: Show or change clank config for this agent + repo.
---

User invoked /clank with arguments: "$ARGUMENTS"

- If $ARGUMENTS is empty: run `clank status --short` via Bash, print
  output, then print: "For an interactive picker, run `/clank
  config`."
- If $ARGUMENTS is "config": use the structured-question tool
  (`AskUserQuestion` in claude, equivalent in codex) to present:
  - "Enable auto-mode (hint, default)" → `clank auto on`
  - "Enable auto-mode (wait, blocking)" → `clank auto on --mode wait`
  - "Disable auto-mode" → `clank auto off`
  - "Switch role to master" → `clank auto --role master`
  - "Switch role to reviewers" → `clank auto --role reviewers`
  - "Set wait timeout" → ask for value, then `clank config set
    wfw_timeout <value>`
  - "Change agent label" → ask for value, then `clank as <value>`
  Run the matching command via Bash; reprint state.
- Otherwise: run `clank $ARGUMENTS` via Bash; print output.

No commentary; just the command output (plus the one-liner hint
in the bare case).
```

### D7. Continuation prompt format

**Question.** What text does the hook send back to the agent when
wfw returns items?

- **Option A — Raw wfw JSON**: `{"items":[...]}` dumped in.
- **Option B — Short human instruction**: "wfw returned 2 items. Run
  `clank status` and act on the first one." (Agent re-reads status to
  see details.)
- **Option C — Rendered work summary + paths**: "Master needs to
  revise plan X at sha Y. Open `.clank/plans/X.md` and apply
  feedback at `.clank/agents/codex/feedback/X/abc1234.md`."

**Recommendation: C, with the structured items appended as JSON for
the agent to parse if it wants.** The agent doesn't need to re-shell
`clank status` if the hook already has the answer. Including paths
inline means the agent's first move is `Read` not `Bash`, which is
faster and uses less context.

### D8. Setup idempotency — how does re-running `clank setup` behave?

**Question.** Files like `~/.claude/settings.json` and
`~/.codex/hooks.json` are user-owned and may contain unrelated hooks.

- **Option A — Refuse to overwrite if drifted** (what `init.rs` does
  for `.gitignore`). Pro: safe. Con: fails noisily on settings files
  that legitimately contain other hooks.
- **Option B — Merge by marker key**: every clank-installed hook gets
  `"id": "clank-stop-hook"`; setup finds-and-replaces that entry and
  leaves others alone. Pro: composes with other tools. Con: more code,
  needs careful JSON merge logic.
- **Option C — Append only**: just add a new entry; on re-setup, add
  another one. Con: duplicates pile up.

**Recommendation: B for settings/hooks files; A for skill files.**
Skill files (`SKILL.md`) we own outright; refusing on drift is fine
and matches `init.rs`'s contract. Settings files we share with the
user and other tools; tagged-merge is the only safe option.

### D9. Status display while wfw is waiting (+ claude cancellation risk)

**Update from E1 (both agents tested, conclusion).** Blocking
long-poll hook is viable for **both** claude and codex.

- **Claude**: shows `Ran 1 stop hook (ctrl+o to expand)` row
  (expandable to stderr). Continuation works. `stop_hook_active`
  short-circuit works. Cancellation works cleanly. Labels the
  continuation as `Stop hook error:` — functionally correct (that
  IS the documented contract), but the label is misleading. Out of
  our hands.
- **Codex**: shows a live progress indicator while the hook runs
  (nicer than claude's). Continuation labeled `Stop hook (blocked)
  feedback:` — clean. Cancellation works. `stop_hook_active`
  short-circuit works.

Both ship with the blocking long-poll hook (D9 option A). Codex is
the better UX of the two but the gap isn't decision-changing.

Three modes, with **`hint` as the activation default** (the value
`clank auto on` writes by default; users opt into `wait` explicitly):

```json
{ "auto_mode": "off" | "hint" | "wait" }
```

- `off` — hook exits immediately. No auto behavior.
- `hint` (**default for `clank auto on`**) — hook runs a cheap
  status check and routes:
  - If there's work pending FOR THIS AGENT right now → emit a
    continuation prompt with that work (same payload `wait` would
    have produced) and resume the agent.
  - If there's a plan in flight but the wait is on SOMEONE ELSE →
    emit a continuation suggesting `clank wfw` (so the agent can
    explicitly start a blocking wait if it wants to).
  - If nothing at all → exit 0 quietly, let the agent stop.
- `wait` — hook long-polls `clank wfw` directly; blocks the agent
  until work arrives (or `wfw_timeout` elapses; default indefinite).

**Why hint as default:** the experiment validated `wait` works
cleanly but it does block the agent visibly — sometimes for a long
time. `hint` gives the agent a useful nudge on every stop without
ever holding the turn open. If there's already work, instant
continuation; if there isn't, the agent's choice (via the suggested
`clank wfw`) to actually wait. Provisional — revise after dogfooding.

The hint-mode "waiting on someone else" branch is the part that
needs careful design: it has to project status enough to know
WHO it's waiting on without duplicating logic with `derive_work`.
Reasonable approach: re-use `plan_view::project` and check the
`waiting_on` field across plans the agent participates in. If any
plan has `waiting_on != this_agent` AND the agent has a role that
cares about that plan, emit the suggestion.


User said: "if it can while wfw is happening it should show the
current plan status on the terminal." And raised a real follow-on
concern: if claude has no status AND no clean cancel, the hook
becomes hostile — the user can't tell if it's working and can't
escape it without potentially killing the session.

- **Codex**: `statusMessage` shows a single static string while the
  hook runs. We can put `"Clank: waiting for work — run `clank
  status` in another shell for details"` there. Dynamic content not
  possible.
- **Claude**: no equivalent surface; hook stderr/stdout are hidden
  during execution. Cancellation behavior (Ctrl-C / ESC) during a
  long Stop hook is the open question that decides whether we ship
  the hook for claude at all.

Sub-options for claude (decided by experiment, not by guessing):

- **A. Long-poll** — hook calls `clank wfw` with a long timeout
  (e.g. 25 min) and blocks the agent the entire time. Best when work
  arrives during the wait; worst for cancellation / "is it stuck?"
  perception.
- **B. Short long-poll** — same shape but short timeout (e.g. 30s).
  Wins when work is already pending; gracefully gives up otherwise.
  Mitigates cancellation risk by never blocking long enough to matter.
- **C. Hint-only mode (no wait)** — hook does a cheap `clank wfw
  --check` (returns immediately: items if pending, otherwise nothing)
  and emits a continuation prompt ONLY when there's already work
  ("you have N pending items — run `clank wfw`"). Never blocks the
  agent for more than a few hundred ms. Strictly worse than A/B for
  "work arrived 2 min after I stopped" — the agent has already
  given up. Strictly better for "I just committed, is there review
  feedback waiting?" — instant nudge, no perceived freeze.
- **D. Skip the hook in claude entirely** for v1 — claude gets the
  skill + `/clank` only; user types `clank wfw` manually.

A and B are different timeout values, both blocking. C is
non-blocking — basically "did I forget to call wfw?" reminder.
Could ship A/B + C as separate modes, with config choosing one.

**Recommendation: defer, gated on the experiment in §Experiments
below.** Don't commit to A vs B vs C until we've actually fired a
10s test hook in a claude session and watched what happens. Codex
gets `statusMessage` either way.

### D10. `clank init` additions

`clank init` today only writes `.clank/.gitignore` + creates
`.clank/plans/`. User wants it to also create:

- `.clank/agents/.gitignore` (ignore everything except per-agent
  configs? or ignore the whole thing?) — current
  `.clank/.gitignore` is `feedback/\ncache/\n`. New `agents/` is
  agent-owned; we should commit `agents/<name>/config.json` but
  ignore `agents/<name>/feedback/`. → update the existing gitignore
  to add `agents/*/feedback/` and `agents/*/cache/` exclusions.
- `.clank/master.json` — per D5 recommendation, **drop this**. If D5
  is rejected, write an empty `{}` and document the schema.

---

## Recommended architecture (assuming all recommendations accepted)

### File layout after `clank init` + `clank setup`

```
<repo>/.clank/
├── .gitignore                       # updated to cover agents/*/feedback
├── agents/
│   └── <agent>/
│       ├── config.json              # auto-mode + role + timeout
│       └── feedback/                # already exists, gitignored
├── plans/
└── ... (existing)

~/.claude/
├── settings.json                    # merged: clank Stop hook added
└── skills/clank/SKILL.md            # knowledge + /clank command

~/.codex/
├── hooks.json                       # merged: clank Stop hook added
├── skills/clank/SKILL.md            # knowledge skill
└── commands/clank.md                # /clank slash command
```

### Per-agent config (`.clank/agents/<name>/config.json`)

```json
{
  "auto_mode": "hint",         // "off" | "hint" | "wait" — `clank auto on` writes "hint"
  "role": "reviewers",         // "reviewers" | "master"
  "wfw_timeout": null          // null = indefinite (default); else "30m" / "5m" / etc.
}
```

Schema lives in `clank_core` as a `serde` struct; one source of truth
read by the stop-hook adapter, written by `clank auto`.

**Timeouts — two layers.** Clank's `wfw_timeout` defaults to `null`
meaning the clank-side wait is indefinite. But the agent's hook
runner has its OWN timeout that kills the hook process regardless
(claude default 10 min, codex default 10 min). If clank waits forever
but the agent kills the hook at 10 min, the user sees the hook
silently die with no continuation. So `clank setup` writes a static
very-large hook `timeout` (recommend 24h = 86400 seconds) into the
hook JSON, and the *clank-side* `wfw_timeout` controls actual
give-up time. User overrides `wfw_timeout` to cap the wait; they
don't need to touch the hook config.

### `clank stop-hook --tool <claude|codex>`

Flow:
1. Read hook stdin JSON. If `stop_hook_active == true`, exit 0
   without continuation. (Both agents enforce this.)
2. Resolve agent label: `CLANK_AGENT` env > tool-name default.
3. Read `.clank/agents/<agent>/config.json`. If missing or
   `auto_mode == "off"`, exit 0.
4. Invoke the same projection wfw does, with `role` and `wfw_timeout`
   from config.
5. If no items by timeout, exit 0 (agent stops normally).
6. If items returned, format the continuation prompt (D7 shape) and
   emit per-tool decision JSON to stdout. Exit 0.

The hook never fails the agent. Any internal error → exit 0 with a
diagnostic on stderr.

### `clank auto on|off|status [--as <name>] [--role master|reviewers]`

Writes/reads `.clank/agents/<resolved-name>/config.json`. `status`
prints the current state in a human format (and JSON with
`--json`). The `/clank` skill in both agents shells out to this.

### `clank setup [--user] [--repo <path>]`

Default: install user-scope files only (`~/.claude/`, `~/.codex/`).
With `--repo`, also seed per-repo agent configs (currently a no-op
since `clank auto on` does that).

Writes:
- `~/.claude/skills/clank/SKILL.md` (refuse if drifted)
- `~/.codex/skills/clank/SKILL.md` (refuse if drifted)
- `~/.codex/commands/clank.md` (refuse if drifted)
- Merges Stop hook entry tagged `"id":"clank-stop-hook"` into
  `~/.claude/settings.json` and `~/.codex/hooks.json` (replace by id
  on re-run).

Prints what got changed and what was left alone.

### `clank init` changes

- Update embedded `.gitignore` body to also cover
  `agents/*/feedback/` and `agents/*/cache/` (or just `agents/*/`
  with `!agents/*/config.json` carve-out — decide at impl time).
- Per D5 recommendation: do NOT create `.clank/master.json`. If D5
  rejected, also write `{}` with a comment about schema.

### Skill content (both tools, same text)

```yaml
---
name: clank
description: Multi-agent peer review around plans + wait-for-work.
---

Clank is a peer-review workflow tool. Each repo's `.clank/`
directory contains:
- plans/    — active plans (one md file per plan)
- agents/<name>/feedback/<plan>/<commit>.md — per-agent review notes
- finished/ — finalized plans

Key commands you can run via Bash:
- `clank status` — show current plan + gate state
- `clank wfw` — long-poll for the next thing this agent should do
- `clank finish <plan>` — finalize an approved plan

When the Stop hook returns work, act on it immediately. The hook
will keep the turn open until wfw produces something or times out.

To toggle auto-mode in this repo: `/clank auto on` or
`/clank auto off`.
```

(Final wording TBD; this is just shape.)

---

## Experiments (run before impl)

### E1. Stop-hook UX smoke test

Setup at `/tmp/clank-hook-experiment/` — two isolated projects with
a fake hook that delays 10s and then resumes the agent with a
"reply with CLANK_HOOK_OK" continuation prompt. Tail
`/tmp/clank-hook-experiment/hook.log` while it runs.

For each of {claude, codex}, answer:

1. **Visibility** — During the 10s wait, is there ANY visible cue
   that the agent is intentionally paused on a hook (vs. crashed)?
   (Codex `statusMessage` should show; claude unknown.)
2. **Cancellation** — Ctrl-C / ESC / Ctrl-D during the hook. Does
   the session survive? Hook child killed cleanly? Agent usable
   after?
3. **Continuation** — Does the agent actually receive the
   "CLANK_HOOK_OK" prompt after the wait and respond to it?
4. **Loop guard** — Confirm the second invocation (the one fired
   when the agent stops after responding to CLANK_HOOK_OK)
   short-circuits via `stop_hook_active=true`. Log line should show
   `stop_hook_active=true short-circuit`.

Outcome decides D9 sub-option (A long timeout / B short timeout /
C skip claude hook entirely). If cancellation in claude really
terminates the session, we ship C in claude or B with a < 60s
timeout.

Run via the README at `/tmp/clank-hook-experiment/README.md`.
Cleanup: `rm -rf /tmp/clank-hook-experiment`.

## Open questions for the user / next reviewer

(All previous opens resolved. Ready for codex review.)

## Acceptance sketch

- `clank setup` on a fresh user account writes the four files +
  merges two hook configs. Re-running is idempotent.
- `clank init` in a fresh repo writes `.gitignore` covering the
  agent-feedback paths.
- `clank auto on` in a repo writes
  `.clank/agents/<tool>/config.json` with `auto_mode: "wait"`.
- After setup + `clank auto on`: a claude session that finishes a
  turn invokes the stop hook, the hook polls `clank wfw` until
  timeout, and on returned work resumes the agent with the
  continuation prompt without the user typing anything.
- Same flow works under codex with the codex-shape decision JSON.
- The hook short-circuits when `stop_hook_active == true`; no
  infinite loop / 8-cap force-stop in claude.
- The hook never fails the agent — `clank` missing, config missing,
  wfw error all exit 0.
- `/clank` works in both tools; `/clank auto off` flips the bit
  visible to the next stop hook invocation.

## Out of scope (intentionally)

- Durable background waiter (the original
  `agent-automation-hooks.md` proposal). The Stop hook IS the
  waiter; recovering across a killed turn is just "user re-invokes".
- Post-commit git hook for notifying clank. The watcher already
  picks up commits; redundant.
- Publishing as official plugins in claude-plugins-official or the
  codex marketplace.
- Live status display in claude during long-polling (separate plan,
  if ever).
- Multi-agent-per-tool support beyond the `--as <name>` override.
