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

## Skill content

**clank-master** (`description`: you are the MASTER in a clank repo):
- the master loop: take wfw work → implement plan milestones → commit
  per milestone → address review feedback.
- queue promote (EVALUATE readiness, don't blind-promote), plan
  authoring/rescoping, `clank finish`, shelve / unshelve / `purge
  --drop`.
- roster management: `clank agent add` / `remove` / `set-master` /
  `list` (NEW — currently undocumented anywhere).
- blocks (ask the human when contentious / drifting from intent).
- understands verdicts as INPUT (reads feedback, acts on gate state) but
  never WRITES them.

**clank-reviewer** (`description`: you are a REVIEWER in a clank repo):
- the reviewer loop: take wfw work → review the commit → run the exact
  `clank feedback write --commit <sha> --verdict … -m "…"`.
- the verdict semantics (APPROVE / FINISHED / REQUEST_CHANGES) — today
  the bulk of the shared file — live HERE.
- never runs finish / promote / roster commands.

**shared core** (both): clank intro; `.clank/` layout; `status` / `wfw`
/ `as` / `auto`. Drift fixes folded in: `config.json` is a ROSTER of
agents-with-roles (NOT "designated master agent"); `clank auto --role`
is a no-op (note it).

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
- `clank-pr-review` unchanged.
