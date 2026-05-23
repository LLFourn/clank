# Clank ↔ Claude / Codex Integration

## Summary

Wire clank into Claude Code and Codex CLI so agents call `clank wfw`
without being prompted. Deliverables:

1. A versioned **clank skill** installed into both agents so they know
   what clank is and how its workflow works.
2. A **Stop hook** for each agent that polls `clank wfw` (hint or
   wait mode) when the agent would otherwise end its turn, and
   resumes the agent with the returned work as a continuation
   prompt.
3. A **`clank setup`** command that writes those files and a
   **`/clank`** slash command (in both agents) for managing config
   without editing JSON by hand.
4. A **`clank doctor`** command that checks every setup invariant
   (repo, user-scope, current session) so users can self-diagnose
   when something doesn't fire.

Plus the related smaller pieces: `clank init` adds the right
gitignores + claude edit-permission rules, and (when run inside
an agent) interactively bootstraps that agent's identity;
per-agent state — auto-mode, wfw_timeout, current session binding
— lives in `.clank/agents/<name>/config.json`; repo-wide master
designation in `.clank/config.json`.

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
- **Option B — Session bound in agent config** at
  `.clank/agents/<label>/config.json` (gitignored), with a
  `session: { id, tool, updated_at }` field. `/clank as alice`
  writes alice's config with the current session; hook iterates
  agent configs looking for a match. True per-session identity,
  state lives next to the agent it identifies.
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
the appropriate env var, writes
`.clank/agents/alice/config.json` with
`session: { id: <env-id>, tool, updated_at: now }`. Hook reads the
same id from stdin and iterates agent configs to find the match.

**Bootstrap UX:**
- Fresh install: no agent config bound to this session →
  resolver errors `NoAgentForSession`. Caller (hook / wfw /
  status) surfaces "no agent set up for this session — run
  `clank init` (inside this session) or `clank as <label>` to
  bind".
- User wants custom name later: `clank as alice` → writes
  alice's config with this session binding (and clears the same
  binding from any other agent that held it).
- Cleanup: when an agent's binding is overwritten (e.g.
  `clank as alice` then `clank as bob` in the same session),
  the older agent's `session` field is cleared. No background
  prune needed — stale bindings just stop matching anything.

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

### D5. Role designation — per-agent flag vs. repo-level config

**Question.** How does the system know which agent is "the master"
for a repo, and which are reviewers?

- **Option A — Per-agent role field** in each
  `.clank/agents/<n>/config.json`. Any agent can self-declare as
  master; two agents could declare themselves master
  simultaneously.
- **Option B — Repo-level config file** (`.clank/config.json`
  naming the master; everyone else is reviewer by inference).
- **Option C — Both with reconciliation**.

**Recommendation: B.** A repo has one master (by intent of the
clank workflow); the model should reflect that. Single source of
truth, no two-agents-both-master class of bug.

Schema (extensible — only the `master` field exists in v1):

```json
// .clank/config.json
{
  "master": "alice"
}
```

Optional file. If absent → no master designated yet; operations
that need a master (like wfw with auto-inferred role) default to
`reviewers` for the calling agent. Explicit `--role master` or
`clank auto on --role master` claims it.

`clank auto on --role master` is the canonical setter — it writes
both this agent's auto-mode AND updates config.json's `master`
field to name this agent. `clank auto on --role reviewers` clears
the `master` field IF this agent currently holds it (and writes
auto-mode); otherwise it just writes auto-mode.

**Consequence for per-agent config.** `role` no longer lives in
`.clank/agents/<n>/config.json`. The schema shrinks to:

```json
{ "auto_mode": "hint", "wfw_timeout": null }
```

Role comes from comparing label vs. `config.json.master`.

### D6. The `/clank` slash command — scope and shape

**Question.** What does `/clank` do, and how is it invoked?

**Goal: discoverability.** Users shouldn't have to memorize CLI
args. `/clank` is the discovery surface.

**Constraint.** Slash commands in both agents capture shell stdout
into prompt context — they don't hand off the terminal to the
subprocess. So the only way to give the user a picker UI is to
have the skill body tell the model to call its own structured-
question tool (claude `AskUserQuestion`; codex's
`elicitation_request` / MCP-elicit). That's an LLM-mediated
picker. (Verified against both agents' binaries + bundled
plugins — see Out of Scope for why nothing fancier is possible
today.)

- **Option A — Passthrough only**: `/clank auto on` maps to `clank
  auto on`. No bare `/clank` menu. Discoverability: nil.
- **Option B — Model-mediated picker**: skill body grounds the agent
  in (current state, available options, command to apply each) and
  asks the agent to use its structured-question tool. Slow (LLM
  turn before the picker shows), but works today and gives a real
  option-picker UI.

**Recommendation: B.** Skill body + structured-question tool. Spec
below.

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

### D10. `clank init` + gitignore additions

`clank init` today writes `.clank/.gitignore` + creates
`.clank/plans/`. New work:

**Two gitignore files to keep in sync.** The repo root already has
its own gitignore that wholesale-ignores `.clank/*` with positive
carve-outs:

```text
# <repo>/.gitignore (existing)
/target/
.clank/*
!.clank/plans/
!.clank/finished/

/.claude/
.clank/feedback/
.clank/cache/
```

After this plan, the ROOT gitignore needs additional carve-outs
for `config.json` and `agents/`:

```text
# <repo>/.gitignore (target)
/target/
.clank/*
!.clank/plans/
!.clank/finished/
!.clank/config.json
!.clank/agents/

/.claude/
.clank/feedback/                    # legacy; gone after migration
.clank/cache/
.clank/agents/*/config.json         # per-user, not committed
```

And the managed `.clank/.gitignore` mirrors (so cloning into a
fresh worktree without the root-gitignore update still ignores
the right sub-paths):

```text
# .clank/.gitignore (managed by clank init)
feedback/                           # legacy
cache/
agents/*/config.json
```

Note: `agents/*/feedback/` is NOT ignored — peer reviews are
tracked (every contributor sees them).

**`clank init` responsibilities:**
- Write/update `.clank/.gitignore` to the body above (existing
  drift-refusal contract from `init.rs` continues to apply).
- Detect ROOT `.gitignore` and, if the carve-outs are missing,
  warn the user (don't auto-edit the root gitignore — it's
  user-owned and might have other rules). The `init.rs`
  `warn_if_globally_excluded` plumbing already does similar
  probing; extend it.
- Do not create `.clank/config.json` (it's optional; written by
  `clank auto on --role master`).

**Test** (extends the existing `init.rs` test suite): a
`check-ignore`-based test verifying that for a freshly-initialized
repo with the recommended root gitignore, `git check-ignore`
classifies the following correctly:
- `.clank/config.json` → TRACKED
- `.clank/agents/<n>/config.json` → IGNORED (per-user)
- `.clank/agents/<n>/feedback/<plan>/<commit>.md` → TRACKED
- `.clank/cache/anything` → IGNORED

Each path covered with both positive and negative assertions
(check-ignore exit 0 with source line, vs. exit 1 = not ignored).

**Agent edit-permission setup (also `clank init`'s job, claude
only).** Without explicit permission rules, claude prompts the
user on every Write/Edit into `.clank/agents/<name>/` — death by
a thousand prompts. `clank init` writes a blanket allow rule.

**Claude only.** Codex's permission model is shell-command-based
(`prefix_rule(pattern=[...])` matches argv arrays for command
invocation, not file paths). Codex file access is governed by its
sandbox mode (`workspace-write` etc.), which by default permits
writes inside the workspace root — so writes to
`.clank/agents/...` already work without configuration. No codex
permission file written by `clank init`.

For claude, write to `.claude/settings.local.json` (the
conventional per-user-per-repo settings file) under
`permissions.allow`:

```json
{
  "permissions": {
    "allow": [
      "Write(.clank/agents/**)",
      "Edit(.clank/agents/**)",
      "Read(.clank/agents/**)"
    ]
  }
}
```

`clank init` **does** write/update this file unconditionally —
no opt-in flag, no warning. The user can revert or move the rule
later if they want.

**Clank takes no position on whether `.claude/` is committed or
gitignored.** Some repos commit `.claude/`; some don't. `clank
init` does not edit, warn about, or recommend changes to the
user's root gitignore. The `settings.local.json` filename is the
standard convention for per-user overrides, so it works whether
the repo commits `.claude/` (in which case `.local.json` is
typically already gitignored) or ignores `.claude/` entirely.

The blanket `.clank/agents/**` scope is deliberate: any agent can
edit ANY agent's dir, because all agents run as the same OS user
— the "agent label" is attribution, not a security boundary.

`.claude/settings.local.json` is merged tagged-key style (same
approach as D8) so we don't clobber unrelated allow rules the
user may have added.

Test: `clank init` in a fresh repo produces
`.claude/settings.local.json` with the expected three allow
entries; re-running is idempotent; pre-existing unrelated rules
are preserved.

---

## Recommended architecture (assuming all recommendations accepted)

### File layout after `clank init` + `clank setup`

```
<repo>/.clank/
├── .gitignore                       # updated: cache + agents/*/config.json ignored
├── config.json                      # { master: "<label>" } — TRACKED
├── agents/
│   └── <agent>/
│       ├── config.json              # auto-mode + session binding (GITIGNORED)
│       └── feedback/                # TRACKED (peer reviews)
├── cache/                           # gitignored entirely
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

Per-user, per-machine settings for this agent in this repo.
**Gitignored** (auto-mode preference and session_id are not
repo-shared concerns):

```json
{
  "auto_mode": "hint",              // "off" | "hint" | "wait"
  "wfw_timeout": null,              // null = indefinite; else "30m" / "5m"
  "session": {                      // null when not yet bound (fresh agent)
    "id": "742f6a04-f174-409a-ab01-419a16c5f372",
    "tool": "claude",               // "claude" | "codex"
    "updated_at": "2026-05-23T16:24:47+10:00"
  }
}
```

Notes:
- No `role` field — role is derived from `.clank/config.json`.
- `session` is the binding written by `clank as <label>` (or the
  first run of `/clank as alice`). The hook resolver iterates
  agent configs looking for one whose `session.id` matches the
  hook stdin's `session_id`.
- When `/clank as alice` runs in a new session, alice's config is
  updated — old binding is overwritten. (One label, one active
  session at a time; matches reality.)

Schema lives in `clank_core` as a `serde` struct; the CLI does
the file I/O.

### Repo config (`.clank/config.json`)

Repo-level settings. **Tracked** in git (this IS a repo-shared
concern — every contributor needs to know who the master is).

```json
{
  "master": "alice"
}
```

Optional. Only field in v1 is `master` — the agent label
designated as master for this repo. Role inference: an agent's
role is `master` iff its resolved label equals `config.master`;
otherwise `reviewers`. If `config.json` is missing or `master`
is absent, no agent is master.

Typed `RepoConfig` struct in `clank_core`; pure helper
`role_for(label: &AgentLabel, config: Option<&RepoConfig>) -> Role`.
Schema is extensible; future fields layer in via additional
optional `serde` fields.

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

### Identity resolver (shared by stop-hook, auto, `clank as`)

**Crate split.** `clank_core` stays pure: it owns the types
(`Tool`, `AgentLabel`, `AgentConfig`, `RepoConfig`, `Session`,
`IdentityInputs`) and the pure resolver function. The CLI does
all I/O — reads stdin, env, and agent config files; constructs
`IdentityInputs`; calls the resolver.

Pure resolver in `clank_core`:

```text
fn resolve_agent_identity(inputs: &IdentityInputs)
   -> Result<AgentLabel, ResolveError>

precedence:
  1. inputs.explicit_label (from CLANK_AGENT env)              → Ok
  2. if inputs.session_id is Some:
       for each (label, agent_config) in inputs.agent_configs:
         if agent_config.session.id == inputs.session_id
            AND agent_config.session.tool == inputs.tool       → Ok(label)
       → Err(NoAgentForSession)
  3. inputs.session_id is None                                 → Err(NoSession)

enum ResolveError {
  NoSession,         // not running inside an agent (or hook),
                     // and no CLANK_AGENT override
  NoAgentForSession, // running inside an agent, env session_id
                     // present, but no agent config has bound it
}

struct IdentityInputs {
  tool: Tool,
  explicit_label: Option<AgentLabel>,
  session_id: Option<SessionId>,
  agent_configs: Vec<(AgentLabel, AgentConfig)>,
}
```

**No tool-name fallback.** If the resolver can't find an agent
for the current session, the caller MUST surface the error;
silently defaulting to label `claude` / `codex` was a
foot-gun (you'd end up writing feedback as the wrong agent).

CLI side (in `crates/cli`):

```text
build_identity_inputs(tool, hook_stdin: Option<HookInput>, repo) -> IdentityInputs
  reads CLANK_AGENT env (if set)
  resolves session_id:
    if hook_stdin.is_some()  → hook_stdin.session_id
    else if tool == claude   → env CLAUDE_CODE_SESSION_ID
    else if tool == codex    → env CODEX_THREAD_ID
  loads every .clank/agents/<label>/config.json into agent_configs
  returns IdentityInputs { tool, explicit_label, session_id, agent_configs }
```

Writer side (CLI):

```text
clank as <label>
  1. read session_id from env (auto-detect tool from which env var
     is set: CLAUDE_CODE_SESSION_ID → claude; CODEX_THREAD_ID →
     codex; error if neither)
  2. load .clank/agents/<label>/config.json (or default empty)
  3. set config.session = { id: <session_id>, tool, updated_at: now }
  4. write atomically (temp + rename)
  5. if any OTHER agent's config holds this session id, clear that
     agent's session field (one session can't be bound to two
     labels simultaneously)
```

`AgentConfig` is a typed serde struct in core; both reader
(stop-hook, auto, wfw) and writer (`clank as`) share it. Unit
tests in core pin resolver precedence; integration tests in CLI
pin the file round-trip and the "clear stale binding" rule.

Why this split matters: core compiles to wasm unchanged (no
filesystem, no env, no clock) and is testable without temp dirs.
The CLI is the only place that knows about config files on disk.

### `clank stop-hook --tool <claude|codex>`

Flow:

1. Read hook stdin JSON into a typed `HookInput` struct (`session_id`,
   `cwd`, `stop_hook_active`, ...). On parse failure → exit 0 with
   a diagnostic on stderr.
2. If `stop_hook_active == true` → emit nothing, exit 0. (Both
   agents enforce this; we just MUST honor it.)
3. Resolve label via `resolve_agent_identity(tool, Some(input),
   env, repo)`.
   - `Err(NoAgentForSession)` → exit 0; stderr =
     "no agent set up for this session — run `clank init`
     (inside this session) or `clank as <label>`". DO NOT
     fall back to a default.
   - `Err(NoSession)` is impossible here (hook stdin always
     carries `session_id`); treat as bug → exit 0 with stderr.
4. Read `.clank/agents/<label>/config.json` into a typed
   `AgentConfig` struct. (Always exists; resolver only returns
   labels whose config currently holds this session id.)
5. **Branch on `auto_mode`** (this is the state machine codex asked
   for — explicit, exhaustive):
   - `"off"` → emit nothing, exit 0.
   - `"hint"` → run a NON-BLOCKING projection of wait state (no
     watcher, no loop). Compute:
     - `pending_for_me`: items derive_work would return for
       (label, role) right now
     - `pending_for_others`: any active plan whose `waiting_on`
       projects to an agent other than me, where I'm a
       participant
     If `pending_for_me` non-empty → emit continuation per-tool
     (see table) with the rendered work summary (D7). If empty
     but `pending_for_others` non-empty → emit continuation with
     a "you may run `clank wfw`" suggestion. If both empty →
     emit nothing, exit 0.
   - `"wait"` → call the same blocking wfw loop with `role` and
     `wfw_timeout` from config. On returned items → emit
     continuation with rendered work summary. On timeout → emit
     nothing, exit 0.
6. Format continuation EXACTLY per the table below.

**Per-tool output contract** (typed `HookContinuation` struct +
per-tool writer, NOT ad-hoc JSON formatting):

| Branch                       | Claude                       | Codex                                                       |
| ---                          | ---                          | ---                                                         |
| Continuation with `reason`   | exit 2; stderr = reason text | exit 0; stdout = `{"decision":"block","reason":"<text>"}`   |
| `stop_hook_active == true`   | exit 0; no output            | exit 0; no output                                           |
| `auto_mode == "off"`         | exit 0; no output            | exit 0; no output                                           |
| Hint mode, nothing pending   | exit 0; no output            | exit 0; no output                                           |
| Wait mode, timeout elapsed   | exit 0; no output            | exit 0; no output                                           |
| Internal error               | exit 0; stderr = diagnostic  | exit 0; stderr = diagnostic                                 |
| Identity unresolvable (NoAgentForSession) | exit 0; stderr = "no agent set up for this session — run `clank init`" | same |

**Hook exit-code invariant** (precise — codex called out an
earlier blanket "never non-zero" wording that contradicted the
table above):

- **Claude continuation** intentionally exits **2**. That IS the
  agent's continuation protocol. The table is the source of truth.
- **Codex continuation** exits **0** with `decision:block` JSON on
  stdout. Codex's continuation protocol uses stdout shape, not
  exit code.
- **All non-continuation paths** — `stop_hook_active`, auto off,
  no-work, timeout, internal error, identity unresolvable — exit
  **0**.
- **Unexpected failure codes (1, panic, etc.) are bugs.** Tests
  assert the exact table; nothing else.

Unit tests live in `crates/cli/tests/stop_hook.rs`, table-driven
over (auto_mode, has_pending_for_me, has_pending_for_others,
tool, stop_hook_active) → (exit_code, stdout, stderr).

### `clank auto on|off [--mode hint|wait] [--role master|reviewers]`

Writes `.clank/agents/<resolved-label>/config.json` via the shared
typed `AgentConfig`. Identity resolves via the shared resolver
(no hook stdin available; reads session_id from env).

**`clank auto` requires identity to already resolve.** It is NOT
a bootstrap path. If the resolver returns `NoAgentForSession` /
`NoSession`, `clank auto` errors with: "no agent set up for this
session — run `clank init` (inside this session) or
`clank as <label>` first." Bootstrap is exclusively the job of
`clank init` phase 2 and `clank as`.

- `clank auto on` → `auto_mode: "hint"` (default); `--mode wait`
  overrides to `"wait"`.
- `clank auto off` → `auto_mode: "off"`.
- `--role master` → also updates `.clank/config.json`'s `master`
  field to `<resolved-label>` (creating the file if absent).
  Overwrites any prior master designation with a one-line
  "master changed from X to Y" note on stderr.
- `--role reviewers` → if `.clank/config.json.master ==
  <resolved-label>`, clear it (this agent is no longer master).
  If not, no-op for the repo config. Auto-mode is still written
  either way.
- `clank auto status [--json]` → prints current `AgentConfig`
  AND inferred role (with reference to whether config.json names
  this agent as master).

### `clank as <label>`

Writes the session binding into
`.clank/agents/<label>/config.json` (spec'd above). Reads
`CLAUDE_CODE_SESSION_ID` / `CODEX_THREAD_ID` from env;
auto-detects tool from which one is set (errors if neither —
"run inside claude or codex"). Clears stale `session` fields on
other agents holding the same session id.

### `clank wfw` changes — `--author` becomes optional

Today `wfw --author <label>` is required. With the identity
resolver in place, that's redundant — the agent invoking wfw has
the same env (`CLAUDE_CODE_SESSION_ID` / `CODEX_THREAD_ID`) the
hook does, and the same agent-config session lookup applies.

After this plan:
- `--author <label>` → still works, wins if passed (explicit
  override).
- `--author` omitted → CLI builds `IdentityInputs` from env (no
  hook stdin), calls the shared resolver, uses the result on
  success.
- On `Err(NoAgentForSession)`: error with "no agent set up for
  this session — run `clank init` or pass `--author <label>`".
  No fallback to tool name.
- On `Err(NoSession)` (running outside any agent, no env var):
  error with "no session detected — pass `--author <label>` or
  run inside claude/codex".

Similarly, `--role`:
- `--role <role>` → still works, wins if passed.
- `--role` omitted → read `.clank/config.json` and compare
  resolved label against `config.master`. Master if equal,
  reviewers otherwise. If `config.json` is missing or `master`
  is unset, default to `reviewers` (so wfw works for a fresh
  repo's first agent without a master designation; explicit
  `--role master` or `clank auto on --role master` is the path
  to claim it).

This makes the hint-mode "you may run `clank wfw`" suggestion
zero-friction: the agent literally just runs `clank wfw` —
no remembering which `--author` to pass, no looking up its own
label.

### `clank doctor`

Diagnostic command that checks every invariant `clank init` and
`clank setup` are supposed to maintain, plus runtime context
(which tool is currently running, whether the current session
has an identity binding). Output is a flat list of OK / WARN /
FAIL lines; exits 0 if all OK/WARN, 1 if any FAIL.

Checks (in order):

**Repo-scope (only if cwd is inside a git repo with `.clank/`):**
- `.clank/.gitignore` exists with the expected body
- Root `.gitignore` has `!.clank/agents/` carve-out (WARN if missing)
- `.clank/agents/` exists (OK if not — gets created lazily)
- For each agent in `.clank/agents/`:
  - `config.json` parses as a valid `AgentConfig`
  - auto_mode is `off` / `hint` / `wait`
- `.clank/config.json` if present:
  - parses as a valid `RepoConfig`
  - the agent named in `master` has a dir under `.clank/agents/`
    (WARN if not — master designated for an agent that's never
    run locally; harmless but suspicious)
- (No separate session cache — session bindings live in each
  agent's `config.json`.)
- `.claude/settings.local.json` has the `Edit`/`Write`/`Read`
  permission rules for `.clank/agents/**` (WARN if missing —
  user will get prompted on every feedback write)

**User-scope:**
- `~/.claude/skills/clank/SKILL.md` exists and matches embedded
  expected content (FAIL if drifted — user edited it; `clank
  setup --force` would overwrite)
- `~/.codex/skills/clank/SKILL.md` same
- `~/.codex/commands/clank.md` same
- `~/.claude/settings.json` contains a Stop hook entry tagged
  `"id":"clank-stop-hook"` pointing at `clank stop-hook --tool
  claude`
- `~/.codex/hooks.json` same for codex
- `clank` binary on PATH matches the binary actually invoked by
  the hooks (resolve via `which clank` vs the command in each
  hook; WARN if hooks point at a different path)

**Session-scope (only if invoked from inside an agent):**
- Detect tool from env (`CLAUDECODE=1` → claude; `CODEX_THREAD_ID`
  set → codex)
- Session env var present (`CLAUDE_CODE_SESSION_ID` /
  `CODEX_THREAD_ID`)
- Identity resolution: print whether `CLANK_AGENT` env override
  fired, OR which agent's `config.json.session` matched this
  session id, OR FAIL with "no agent set up for this session —
  run `clank init` or `clank as <label>`"
- If resolved, print the role (compare against
  `.clank/config.json.master`): "master" or "reviewers"

Implementation: each check is a small typed function returning
`CheckResult { status: Ok | Warn | Fail, message: String }`.
Aggregated in a `Vec` and rendered. Easy to add new checks over
time. Most checks reuse the same loaders the rest of the CLI
uses (typed `AgentConfig`, `RepoConfig`) — `clank doctor`
catches bit-rot when those loaders diverge from on-disk reality.

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

`clank init` is now a two-phase command:

**Phase 1 — scaffolding (always runs, non-interactive):**
- Update `.clank/.gitignore` to cover `cache/` and
  `agents/*/config.json`.
- Warn if the ROOT gitignore lacks `!.clank/config.json` and
  `!.clank/agents/` carve-outs.
- Write `.claude/settings.local.json` with the agent edit-
  permission rules (per D10).

**Phase 2 — agent identity bootstrap (only if running inside an
agent):** Detect tool from env (`CLAUDECODE` / `CODEX_THREAD_ID`).
If detected:
- Read session id from env (`CLAUDE_CODE_SESSION_ID` /
  `CODEX_THREAD_ID`).
- Prompt: `Agent name [<tool>]: ` — accept default if user hits
  enter.
- Prompt: `Make this agent the master for this repo? [y/N] ` —
  read y/n.
- Write `.clank/agents/<name>/config.json` with the session
  binding (and `auto_mode: "off"` default — user enables later
  via `clank auto on`).
- If master: write `.clank/config.json` with `master: <name>`.
- Print summary of what was written.

If NOT running inside an agent (env vars absent), skip phase 2
with a message: "Not running inside an agent — agent identity
will be bootstrapped on first `clank auto on` from inside
claude/codex."

Non-interactive flag: `clank init --yes` accepts defaults
without prompting (useful for scripts).

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
  merges two hook configs. Re-running is idempotent (tagged merge
  by `"id":"clank-stop-hook"`).
- `clank init` in a fresh repo writes `.clank/.gitignore` per D10;
  warns if the root gitignore lacks `!.clank/config.json` and
  `!.clank/agents/` carve-outs. The `check-ignore` test (per D10)
  asserts:
  - `.clank/config.json` TRACKED
  - `.clank/agents/<n>/config.json` IGNORED
  - `.clank/agents/<n>/feedback/<plan>/<commit>.md` TRACKED
  - `.clank/cache/anything` IGNORED
- `clank init` phase 2 (only when running inside an agent):
  prompts for agent name + master?, writes
  `.clank/agents/<name>/config.json` with the env-derived session
  binding and `auto_mode: "off"`; if master, also writes
  `.clank/config.json`.
- `clank auto on` requires identity to already resolve. Errors
  with "no agent set up for this session — run `clank init` (or
  `clank as <label>`)" if NoAgentForSession. On success: writes
  `auto_mode: "hint"` (default; `--mode wait` overrides). With
  `--role master`, also updates `.clank/config.json`. With
  `--role reviewers`, clears master if this agent held it.
- `clank as alice` writes alice's `.clank/agents/alice/config.json`
  with a `session` field keyed by the env-resolved session_id;
  if any other agent's config held the same session id, that
  stale binding is cleared. Test verifies round-trip + lookup +
  staleness cleanup. (No separate sessions cache; no prune
  needed.)
- After setup + agent bootstrap + `clank auto on` in hint mode:
  a claude session that finishes a turn invokes the stop hook;
  if work is pending for this agent the hook continues claude
  (exit 2 + stderr); if another agent is pending it suggests
  `clank wfw`; if neither it exits 0 silently.
- After setup + bootstrap + `clank auto on --mode wait`: same
  matrix but the hook long-polls instead of one-shot checking.
- Same matrix works under codex (continuation via stdout JSON +
  exit 0).
- `stop_hook_active == true` → hook exits 0 with no output (loop
  guard).
- **Hook exit-code invariant** (matches the table; not a blanket
  "never non-zero"):
  - Claude continuation exits **2** with stderr = reason.
  - Codex continuation exits **0** with stdout =
    `{"decision":"block","reason":...}`.
  - All non-continuation paths AND handled internal errors exit
    **0**. Unexpected exit 1 / panic is a bug.
  - Covered by table-driven tests in
    `crates/cli/tests/stop_hook.rs`.
- `/clank` (bare) prints status + the `/clank config` hint;
  `/clank config` opens the picker; `/clank <verb>` passes
  through. Skill files written by `clank setup` match the
  embedded body.

## Out of scope (intentionally)

- **Any custom TUI shipped with clank** (ratatui or otherwise).
  `/clank`'s discoverability is achieved via the agent's own
  structured-question tool (D6); no separate config picker, no
  in-shell TUI, no native picker that handoffs the terminal.
  Neither claude nor codex exposes a tty-handoff path for slash
  commands, hooks, plugins, or MCP — verified against both
  binaries and ~50 bundled plugins. If this changes, revisit.
- Durable background waiter (the original
  `agent-automation-hooks.md` proposal). The Stop hook IS the
  waiter; recovering across a killed turn is just "user re-invokes".
- Post-commit git hook for notifying clank. The watcher already
  picks up commits; redundant.
- Publishing as official plugins in claude-plugins-official or the
  codex marketplace.
- Live status display in claude during long-polling (separate plan,
  if ever).
- **Nested subagents that don't expose distinct session IDs.**
  Two same-tool top-level sessions in one repo with different
  labels DO work — the session-keyed resolver supports it by
  construction (each session has its own `CLAUDE_CODE_SESSION_ID`
  / `CODEX_THREAD_ID`). What's deferred is the case where one
  agent spawns a subagent that inherits the parent's session id
  (or doesn't surface its own), so `/clank as alice` in the
  subagent would overwrite the parent's identity binding. Detect
  and document if/when this comes up.
