# clank-skill-role-split

Replace the single per-tool `clank` skill with two ROLE-based skills —
`clank-master` and `clank-reviewer` — each built from ONE shared body
(tool-specific bits parameterized) and installed to BOTH `~/.claude` and
`~/.codex`. Fix the accumulated drift along the way. `clank-pr-review`
stays as-is.

## Why

The current `clank` skill jams the master loop and the reviewer loop
into one file, duplicated near-identically across `claude_skill.md` /
`codex_skill.md`. A reviewer is handed master-only noise (finish, queue
promote, plan authoring) and a master is handed reviewer verdict
mechanics. It has also drifted from the roster model.

## Core model

- **Role-based, NOT tool-based.** Splitting by tool is impossible: the
  `claude` tool hosts BOTH the master (`claude`) AND a gate reviewer
  (`ruthless`), sharing one `~/.claude/skills/` install. So BOTH role
  skills install to BOTH tools; the agent picks the one matching its
  roster-resolved role, which `clank status` / `clank wfw` already
  report (the stop-hook hint names the role: "work for `claude`
  (master)"). The skill `description:` frontmatter must make the role
  selection obvious.
- **One shared body per role.** A SKILL.md = shared core (what clank is;
  the `.clank/` layout; `status` / `wfw` / `as` / `auto`) + the
  role-specific loop. Tool differences are PARAMETERIZED, never
  duplicated: the shell word ("via Bash" vs "via shell"), the stop-hook
  phrasing (codex's `Stop hook (blocked) feedback:`), and the
  claude-only `/clank` slash-command block. Exact fragment file layout
  is the implementer's call; the invariant is one source per role with
  tool bits substituted (a `compose_skill(role, tool) -> String`).

## Invariants the skills MUST encode (the heart of this work)

The loops — not the command list — are what agents get wrong today. Each
role body MUST lead with its invariants, MUST/MUST-NOT/STOP-style (caps
for the hard rules, imperative, no hedging — see best practices below).

### Shared (both roles)
- Bind once: `clank as <label>` at session start. Role is ROSTER-derived
  — you are master/reviewer because the repo roster says so, not a flag.
- Act on Stop-hook work IMMEDIATELY, then YIELD. NEVER manually poll
  (`clank wfw` / `status` in a loop) — the Stop hook re-invokes you when
  there is work. You WILL be woken; do not spin.
- Load ONLY the skill for your role; do not follow the other role's
  instructions.

### Master — the commit→yield loop (THE central invariant, currently undocumented)
- After you COMMIT ANYTHING, STOP and yield to the Stop hook for review.
  This includes an implementation milestone, a plan revision, AND
  PROMOTING A PLAN FROM THE QUEUE (promotion is a plan commit that must
  clear intro review). Do NOT keep working past a commit — you will be
  woken when the gate has acted.
- NEVER run `clank finish` on your own judgment. Finalize ONLY when the
  Stop hook hands you a finalize item (the gate reached FINISHED).
- Gate on REVIEWERS, not humans. The queue is an instruction, not a
  question — never block the queue to ask permission to do queued work.
  Use `clank block` only when reviews are contentious, the plan is
  drifting from intent, or the work seems unwise.
- EVALUATE a queue item before promoting (read it, rescope/split if
  needed); promote only when ready — then it is a commit, so yield.
- On REQUEST_CHANGES: address it, commit, yield.
- You never WRITE verdicts; you read feedback and act on gate state.

### Reviewer — scope and the verdict
- You review ONLY committable artifacts — code, tests, docs in the repo.
  That is the ENTIRE scope of your verdict.
- NEVER withhold APPROVE or FINISHED waiting on a manual/external action
  a plan lists as a verification step (a human smoke test, an on-device
  check, a deploy). You cannot observe it and the workflow has no signal
  for its completion — blocking on it stalls the plan forever. If the
  committable work is complete and correct, that is FINISHED **even if a
  human still has to smoke-test it**. (Surfacing external steps to the
  human is the master's concern, never your gate.)
- Write exactly ONE verdict via the exact `clank feedback write --commit
  <sha> --verdict … -m "…"`, then YIELD — you'll be woken for the next.
- Review ARCHITECTURE-FIRST. When you find several issues, step back and
  ask whether they are SYMPTOMS of one wrong/missing model. If so, LEAD
  with the architectural mismatch — name the violated invariant and the
  structural change that makes the whole class of bugs hard to write —
  instead of listing the symptoms. A structural fix beats a symptom
  list. (Cite files/lines, but don't let line-level nits bury the real
  issue.)
- You never run finish / promote / roster / plan-authoring commands.

## Skill content (inventory per role)

**clank-master**: the master invariants above + the commands only a
master runs — `queue promote`, plan authoring/rescoping, `clank finish`,
`shelve` / `unshelve` / `purge --drop`, roster management (`clank agent
add` / `remove` / `set-master` / `list` — NEW, currently undocumented
anywhere), and `clank block`. Verdicts appear only as INPUT it reads.

**clank-reviewer**: the reviewer invariants above + `clank feedback
write` and the verdict semantics (APPROVE = good, mid-flight / FINISHED =
the committable work the plan DESCRIBES is complete & merge-ready, NOT
"plan text written" / REQUEST_CHANGES = something committable must
change). Today these verdict definitions are the bulk of the shared
file; they live HERE now.

**shared core** (both): clank intro; `.clank/` layout; `status` / `wfw`
/ `as` / `auto`. Drift fixes folded in: `config.json` is a ROSTER of
agents-with-roles (NOT "designated master agent"); `clank auto --role`
is a no-op (note it).

## Skill-authoring best practices (apply)

Sources: `code.claude.com/docs/en/skills`,
`platform.claude.com/docs/en/agents-and-tools/agent-skills/best-practices`.
- **Description = the role guard.** Lead with it: "Use ONLY when you are
  the <role> in a clank repo … Do NOT use when you are the <other
  role>." Third person; keep the discriminator short (the listing
  truncates ~1536 chars). This is the PRIMARY lever stopping a reviewer
  from loading the master skill (and running finish/promote it must
  never run).
- **Reinforce selection in-body**: an early role-check line ("If your
  role here is not <role>, stop and use `clank-<other>`."). The
  stop-hook hint and `clank status` already name the role, so the agent
  has the signal.
- **Conciseness**: each SKILL.md is a SINGLE file, well under 500 lines
  (target ~150). At this size DO NOT split into progressive-disclosure
  reference files — keep setup/doctor's one-file-per-skill model. Cut
  anything not acted on.
- **Invariants first, MUST/STOP-style**, caps for hard rules, imperative.
- **Loop language**: "STOP — the Stop hook re-invokes you when there is
  work," NOT "wait for…". No polling instructions.
- **Anti-patterns to avoid**: vague descriptions, wait-without-STOP,
  nested reference files, "do X unless…" hedging, documenting commands
  the role never runs.

## Implementation

- `setup.rs`: replace the two `clank` skill installs with, per tool ∈
  {claude, codex}, `clank-master/SKILL.md` + `clank-reviewer/SKILL.md`
  composed via `compose_skill(role, tool)`. REMOVE the now-obsolete
  `~/.{claude,codex}/skills/clank/` on setup (don't leave a stale skill
  behind) — the only "migration" needed (skills are user-scope; no
  per-repo sweep).
- embedded assets: restructure `setup_assets/` into shared + per-role +
  tool-fragment pieces.
- `doctor.rs`: check `clank-master` + `clank-reviewer` for both tools
  against the composed bodies; flag a leftover `clank` skill dir as a
  warning with the `clank setup --force` fix.

## Testing (respect no-binary-spawning-tests)

Pure string-composition tests:
- `compose_skill(Master, _)` contains `clank finish` + `agent
  set-master`; does NOT contain `feedback write --verdict`.
- `compose_skill(Reviewer, _)` contains `feedback write --verdict` + all
  three verdicts; does NOT contain `clank finish` / `queue promote`.
- **Invariants present** (guard against silent drift of the behaviours
  that motivated this work): the master body asserts the commit→yield
  loop (e.g. contains both "promot" and a STOP/yield instruction); the
  reviewer body asserts the committable-scope rule (never block
  APPROVE/FINISHED on a manual/external step). Assert by stable
  substrings.
- **Role-guard descriptions**: each role's `description` contains "ONLY"
  + its own role and names the other role as excluded.
- tool substitution: claude → "via Bash", codex → "via shell"; the
  `/clank` block is present for claude, absent for codex.
- FINISHED semantics defined ONCE (today's
  `claude_codex_skills_define_finished_identically` test becomes
  structural — a single source, asserted present in the reviewer body).

## Acceptance

- `clank setup --force` writes `clank-master` + `clank-reviewer` to both
  `~/.claude` and `~/.codex`, and removes the old `clank` skill dir.
- `clank doctor` green (new skills, no drift, no leftover).
- Drift gone: roster wording; roster commands documented (master);
  `--role` no-op noted.
- Lean roles: reviewer has no master-only commands; master has no
  verdict-writing mechanics.
- Invariants encoded MUST/STOP-style and lead each body: master
  commit→yield (incl. promotion); reviewer committable-scope (no blocking
  on manual/external steps) + architecture-first review (lead with the
  modeling mismatch, not a symptom list); shared no-poll/act-immediately.
- Role-guard descriptions ("Use ONLY when … Do NOT use when …") so a
  reviewer never loads the master skill.
- `clank-pr-review` unchanged.
