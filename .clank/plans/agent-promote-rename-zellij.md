# agent-promote-rename-zellij

Rename `clank agent set-master` → `clank agent promote`, and make a
promote in a zellij session MOVE the promoted agent's pane out of the
reviewer stack into the master/stage pane (swapping the demoted old master
the other way).

## Part 1 — rename set-master → promote (mechanical, broad)

`clank agent set-master <name>` becomes `clank agent promote <name>`
(same behaviour: set that agent's role to master, demote the previous
master to commit).

- CLI: `crates/cli/src/cli/mod.rs` — `AgentCmd::SetMaster` →
  `Promote`, `AgentSetMasterArgs` → `AgentPromoteArgs`; keep
  `#[command(alias = "set-master")]` on the new variant for a transition
  period so old skills/muscle memory don't hard-error.
- `crates/cli/src/cli/agent.rs` — dispatch + `set_master`/`set_repo_master`
  renamed to `promote`/`promote_repo_master` (or keep the core name,
  rename the shell). Update the module doc.
- **Update every user-facing "clank agent set-master" string** — there
  are ~25 across error messages and assets: `agent_store.rs`,
  `teams_config.rs` (ResolutionError messages), `init.rs`, `fork.rs`,
  `open_zellij.rs`, `pr_review.rs`, `doctor.rs`, `auto.rs`, `main.rs`
  help, and the **skills** (`setup_assets/skill_master.md`,
  `skill_slash_command.md`). Plus the `setup.rs` test that asserts the
  master skill contains `clank agent set-master`, and the mod.rs parse
  test. Grep `set-master` to find them all.
- Note the `promote` overload: `clank queue promote` already exists.
  Different subcommand groups (`agent` vs `queue`) so no clap conflict,
  but call it out so the docs don't confuse the two.

## Part 2 — zellij pane relocation on promote

When `clank agent promote <name>` runs and `$ZELLIJ` is set, the panes
should follow the role change (best-effort, like agent-add-zellij-pane):

- the promoted agent's pane moves from the reviewer STACK to the
  master/STAGE position (the big pane);
- the demoted old master's pane moves from the stage INTO the reviewer
  stack;
- i.e. a SWAP of the two panes' positions, and their titles update
  (`<name> (master)` ↔ `<old> (reviewer)`, via `agent_pane_title`).

Reuse the agent-add-zellij-pane plumbing: find panes by their launch
command (`agent_start_argv` → `terminal_command` from `list-panes
--json`), the `$ZELLIJ` gate, the `.output()`-isolated best-effort
pattern. No-op outside zellij or on any failure (the role change in
config is the source of truth and must persist regardless).

### Implementation risk — likely needs a spike

Moving a pane between the stacked reviewer group and the stage is the
unknown: zellij 0.44 `move-pane` is direction-based and there's no
obvious "pop out of stack / push into stack to an exact slot" verb.
Options to evaluate empirically (mirror the scroll spike approach):
- `move-pane` / `move-pane-backwards` to walk a pane into/out of the
  stack;
- focus + `move-pane` toward the stage region;
- last resort: close + respawn both panes in the new positions (loses
  their sessions — only if no in-place move exists).
Recommend a short spike to find the clean zellij incantation BEFORE
committing the design; if none exists cleanly, ship Part 1 (rename) alone
and leave the relocation as a documented follow-up rather than forcing a
session-destroying respawn.

## Testing (no-binary-spawning)

- rename: `agent promote <name>` parses (and the `set-master` alias still
  parses); `promote_repo_master` sets master + demotes the old one (the
  existing set-master core test, renamed); the master skill body now
  contains `clank agent promote`; no `set-master` left in user-facing
  strings (a grep-style assertion or updated string tests).
- relocation: pure pane-selection helpers (which pane is the current
  master / the promoted reviewer, by launch command) tested against a
  captured `list-panes --json`; the live `move`/`zellij action` calls
  stay untested like the other zellij glue.

## Acceptance

- `clank agent promote <name>` works (with `set-master` as a transitional
  alias); all docs/skills/errors say "promote".
- In zellij, promoting moves the agent's pane to the stage and the old
  master's into the stack (or, if the spike shows no clean in-place move,
  Part 1 ships and relocation is a documented follow-up — never a
  session-destroying respawn).
- Existing tests green; clippy within budget (cli ≤30); fmt clean.
