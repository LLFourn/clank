# contest

**RESEARCH / DESIGN — no code this plan.** Deliverable = this design doc;
the agreed design splits into follow-up implementation plans.

## Goal

`clank contest` benchmarks agent-team compositions. Given a task and a
MATRIX of agent/role combinations, it stands up one CONTESTANT (an isolated
fork) per combination; each contestant's team plans + implements + reviews
+ finishes the SAME task independently. When a contestant FINISHES, the
ORIGINATING session's agents are activated to review its implementation and
leave a NUMERIC SCORE. The human reads the scores and picks the winner
(`clank contest winner <id>`), which grafts that contestant's commit stack
onto main. The matrix IS the experiment: vary the agent set, hold the task
constant, measure the outcome.

Especially compares PLANNING ability — the task is a minimal stub each
master promotes/expands (still through its team's gate). Stub detail is the
knob: minimal → benchmark planning; detailed → benchmark implementation.

## Simplicity first (v1 scope + honest non-goals)

- **Scorers are the originating session's own agents** — no separate judge,
  no blinding, no model-affinity defense (dropped: not worth it unless free;
  a juror scoring work from a team that shares its model is accepted).
- **No automatic winner** — the contest BLOCKS on the human; `clank contest
  winner <id>` is the decision point.
- **No active copy-defenses; passive discovery accepted.** Agents are never
  told they're in a contest (no competitor/benchmark in their context), so
  no ACTIVE incentive to seek rivals. Worktrees share one `.git`, so a
  routine `git branch -a` PASSIVELY lists sibling contestants — accepted v1
  risk (escalate to separate clones if copying is ever observed).
- No K-runs (K=1); no compose-best (single winning stack); no per-contestant
  commit log in the UI (status is 1–2 lines per contestant).

## Contest is a MODE (like `pr-review`)

When `~/.clank/contest/<name>.json` exists, the ORIGINATING session's agents
are in CONTEST MODE: their `wfw` surfaces "score a finished contestant"
work. It's the contest analogue of `pr-review` (where agents review a PR);
a `clank-contest` skill guides the scoring. Removing the marker (on winner
selection / teardown) exits the mode.

- **Reviewers SCORE** each finished contestant (v1 default — confirm):
  they review the contestant's implementation diff + its plan and write a
  numeric score. The MASTER convenes (runs `start`, surfaces status) and is
  the conduit for the human's winner choice.
- **Trigger = incremental** (v1 default — confirm): an originating agent is
  woken to score a contestant AS IT FINISHES, not after the whole field —
  matches the `wfw` model (work arrives when a contestant reaches
  `[stem] finish`). The human compares scores at winner-time.
- **Score scale = /10** (v1 default — confirm).

## Directory structures

**User scope — the mode marker (the `pr-review`-style trigger):**
```
~/.clank/
  contest/
    <contest-name>.json     # { repo: "/abs/originating-repo",
                            #   contest: "<name>", task: "<stem>" }
```
Presence → "this session is judging a contest." The originating agents'
`wfw` reads it to find the contest data in `repo`. Removing it exits the
mode.

**Originating repo — the contest data + scores:**
```
<originating-repo>/.clank/
  contest/
    <contest-name>/
      manifest.json                 # task, base_commit, cap, contestant registry:
                                    #   contestants: [{ id, team:{master,commit:[…],gate:[…]},
                                    #                   worktree, branch, status }]
      <contestant-id>/              # e.g. c0, c1 …  (team recorded in manifest)
        score/
          <originating-agent>.md    # e.g. codex.md, glm.md — review + numeric score
  worktrees/
    contest/<contest-name>/<contestant-id>/   # each contestant's fork worktree (its team works here)
  finished/
    contest/<contest-name>/         # archived contest (manifest + scores) after winner/teardown
```
A contestant's WORK lives in its worktree/branch — the contest dir holds
only the manifest + the originating agents' scores (no duplicated commits).

**A score file** `.clank/contest/<name>/<id>/score/<agent>.md` (reuses the
feedback-body machinery — a numeric header instead of APPROVE/REQUEST_CHANGES):
```
SCORE: 8/10

Crisp plan, tight impl, tests cover the edge cases. -2 for an over-broad
refactor in foo.rs that the task didn't need.
```

## Lifecycle (ENTER / RUN / EXIT)

State lives in git + `.clank/`, NOT a daemon, NOT one long-lived process.
The contest is a set of short-lived commands (`start` / `status` / `winner`
/ `abort` / `clean`) over that state. The originating agents only ever
OBSERVE git/`.clank` output — they never manage the contestant processes.

1. **ENTER — `clank contest start <task> --matrix '<spec>' [--cap N]`.**
   - Reap stale contest state first (Teardown safety). FAIL CLOSED if the
     matrix expands beyond the cap.
   - Expand the matrix → N rosters. Open a dedicated zellij TAB.
   - Per contestant i: `clank fork contest/<name>/c<i>` (worktree) → write
     its roster to `.clank/config.json#/agents` → seed `.clank/stubs/<task>.md`
     + `queue add <task>` → open a PANE STACK in the contest tab running its
     agents in auto-mode.
   - Write `manifest.json` + the `~/.clank/contest/<name>.json` mode marker.
     `start` is FIRE-AND-FORGET — sets up and RETURNS (no held-open process
     to orphan panes).
   - **ATOMIC:** partial setup failure rolls back to a clean slate (don't
     run a partial-matrix benchmark).

2. **RUN — contestants work; originating agents score on finish.**
   - Each contestant's team is autonomous (auto-mode + stop-hook) in its
     worktree → master promotes the stub, team implements + reviews +
     reaches `[stem] finish`.
   - The originating session (contest mode) observes each contestant's
     branch HEAD (→ `[stem] finish` = DONE) + gate/`waiting_on`/blocks (→
     progress). On a finish, the originating reviewers' `wfw` returns "score
     contestant <id>"; each reviews its diff + plan and writes
     `score/<agent>.md`.
   - LIVENESS is the contestant's `waiting_on` + agent-session activity (not
     bare last-commit-time, which false-positives a master mid-turn);
     residual slow-team timeout bias documented. (Robust "agent has stopped"
     detection is a known hard problem, deferred.)

3. **EXIT — human picks the winner; graft; teardown.**
   - The human reads the scores (in `status --tui`) and runs `clank contest
     winner <contestant-id>`.
   - GRAFT the winner's `[stem]` stack + its `.clank/finished/<stem>` trail
     onto main (reuse `purge`/`shelve` rewrite; conflicts if main moved).
   - TEARDOWN per Teardown safety: close the contest tab/panes, remove all
     contestant worktrees, archive the contest + scores to
     `.clank/finished/contest/<name>/`, clear the `~/.clank/contest/<name>.json`
     marker (mode off).
   - `clank contest abort` tears down without grafting.

## Teardown safety (HARD requirement)

Zellij pane-stacks (below) re-enter the N×M-pane shape that caused the
leaked-server incident. Teardown MUST be CRASH-SAFE + IDEMPOTENT across
every exit path (winner, abort, crash, tab close, host reboot). The
`manifest.json` records every worktree + pane + zellij-session id
(record-intent → create → confirm, so a crash mid-create is reapable);
`clank contest clean` reaps leaks idempotently from the manifest and runs
at `start` and on every exit. "Zero leaks across all exit paths incl.
crash" is a gating acceptance criterion.

## Matrix team generation (ad-hoc, NOT named teams)

Cross-product over agent-set AXES from the user-scope agent library
(GH-Actions style): `master ∈ {claude, glm}` × `commit ∈ {[codex],
[codex,glm]}` × `gate ∈ {[ruthless], []}` → the product = N rosters. Also
accepts explicit inline ad-hoc rosters (skip the product). Each roster is
written to its contestant's `.clank/config.json#/agents` (the roster is an
arbitrary agent→role map — no team template). CI-style `exclude`/filter:
deferred.

## Launch: zellij panes, NOT headless

clank's agent loop is interactive (wfw / stop-hook / zellij) with no
headless mode. Each contestant launches as a zellij PANE STACK in the
contest tab via `clank agent start` + auto-mode. One contest TAB, N
stacks (the stated vision). Resource: N×M panes — CAP N (default 4,
`--cap`), FAIL CLOSED above it. Reuses `override-layout` to surface a
contestant's active agent at the top of its stack. Headless is a future
option if N must scale.

## Status integration (1–2 lines per contestant, no commit log)

`clank status --tui` (originating session) shows a CONTEST block:
```
CONTEST: refactor-auth   (3 contestants)
  c0  claude+codex          impl · rd2   waiting: codex      scores: —
  c1  glm+codex+ruthless    DONE                              scores: codex 8  glm 7
  c2  claude+ruthless       planning     waiting: ruthless   scores: —
```
Each line: id + team, phase + round, who it's waiting on (`waiting_on`), and
once DONE the originating agents' scores as they land. `clank contest
status` shows the same, richer (per-contestant gate detail).

## Components → clank primitives

- **Reused:** `clank fork` (worktrees), the roster map, stubs + `queue add`,
  the gate (`compute_gate`) + `finish` (the `[stem] finish` signal), the
  feedback-body machinery (score files), the tagged commit-stack (graft
  unit), the `status --tui` watcher (extended to N worktrees + the CONTEST
  block), `override-layout` (stack surfacing), the pr-review-style MODE
  pattern (the `~/.clank/contest` marker + `wfw` surfacing).
- **New:** the `contest` orchestrator (start/status/winner/abort/clean), the
  matrix expander, contest-mode `wfw` (surface "score contestant X" to the
  originating reviewers), the score-file write path + numeric verdict, the
  graft step, the `.clank/contest/` manifest + `~/.clank/contest` marker, a
  `clank-contest` skill.

## v1 decisions to CONFIRM (flagged for the human + reviewers)

- Score scale `/10`?
- Scorers = originating REVIEWERS only (master convenes + relays the human's
  winner choice), or ALL originating agents incl. the master?
- Trigger = score each contestant incrementally as it finishes, or wait for
  the whole field then score comparatively?

## Deferred / Out of scope

- Headless launch; clones/containers (unless copying observed); K-runs;
  compose-best; matrix `exclude`/filter; beyond-cap batching; robust
  "agent stopped" liveness detection. And all implementation — this is
  research; splits into impl plans (contest orchestrator + zellij launch +
  teardown, matrix expander, contest-mode wfw + scoring, status block,
  graft).

## Acceptance

- The model (originating agents score finished contestants; numeric scores
  in `.clank/contest/<name>/<id>/score/<agent>.md`; scores + progress in the
  originating `status --tui`; manual `winner`; pr-review-style mode) is
  fully specified with directory structures + usage flow.
- Carries the validated invariants: crash-safe idempotent teardown
  (gating), fail-closed cap, atomic ENTER, liveness via `waiting_on`,
  graft-with-trail.
- The three v1 decisions are resolved (here or via review).
- NO code — research only.
