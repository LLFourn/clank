# agent-promote-replaces-set-role

`clank agent set-role <label> <role>` is the wrong shape
for the underlying domain. The invariant is: **a repo has
at most one master.** `compose_kdl` in
`crates/cli/src/cli/open_zellij.rs` errors out with a
"multiple master agents registered" diagnostic if that
invariant is violated. But `set-role` happily lets you
produce that broken state — it rewrites one entry's role
without touching anyone else's. The user then discovers
the breakage at `clank open zellij` time, not at the
moment the bad config was written.

That's a "valid command produces invalid state, error
shows up later" foot-gun. The fix is to remove the
operation that allows it and replace it with one that
preserves the invariant.

## Goal

Replace `clank agent set-role` with `clank agent promote`:

```
clank agent promote <label> [--global]
```

Behavior:

1. If `<label>` is already master → no-op, exit 0 with a
   `note: <label> is already master` message.
2. If `<label>` is a reviewer and there's no current
   master → make `<label>` master.
3. If `<label>` is a reviewer and there IS a current
   master → atomically: set the old master to reviewer
   AND set `<label>` to master. One write, both changes.

That covers every legitimate transition. The illegitimate
ones (two masters; zero masters via demotion) become
unrepresentable in the CLI.

## Why not keep `set-role`

`set-role` has no use case `promote` doesn't cover better:

- `set-role <X> master` when there's no master →
  `promote <X>` does the same.
- `set-role <X> master` when there's another master →
  produces a broken state. `promote` does the right
  thing.
- `set-role <X> reviewer` when X is master → leaves the
  repo with zero masters, which `clank open zellij`
  rejects. Reviewers-only mode isn't a supported
  configuration. The only legitimate "demote current
  master" operation is "and promote someone else", which
  is what `promote <other>` already does.
- `set-role <X> reviewer` when X is already reviewer → no-op.

So no flow regresses.

## Surfaces touched

- `crates/cli/src/cli/agent.rs`:
  - Add `fn promote(args: AgentPromoteArgs)` next to the
    existing `set_role` (~line 590).
  - Logic: load config, find `<label>`, find any existing
    master, mutate both entries, write back. Same
    `--global` / `--repo` flag handling as `set_role`.
  - On no-op (already master): print
    `note: \`{label}\` is already master in {scope}` to
    stderr, exit 0.
  - On promotion: print
    `promoted \`{label}\` to master in {scope} (demoted \`{prev}\`)`
    or `promoted \`{label}\` to master in {scope}` if no
    prior master.
- `crates/cli/src/cli/mod.rs`:
  - Add `AgentCmd::Promote(AgentPromoteArgs)` variant.
  - Remove `AgentCmd::SetRole` and `AgentSetRoleArgs`.
  - Wire `AgentCmd::Promote(a) => promote(a)` in the
    dispatch (~line 37).
- `crates/cli/src/cli/open_zellij.rs:118`:
  - Update the multi-master diagnostic. Today:
    > "Resolve with `clank agent set-role <label>
    > reviewer` to demote a duplicate."
    New:
    > "Resolve with `clank agent promote <label>` to
    > make exactly one of them master (the others
    > automatically become reviewers)."
- `crates/cli/tests/agent_add_remove_integration.rs`,
  `crates/cli/tests/open_zellij_integration.rs`: any
  invocation of `clank agent set-role` becomes
  `clank agent promote`. The semantics map cleanly because
  every existing test that uses `set-role` is either
  promoting a reviewer to master or doing a no-op.

## CLI surface change

The `clank agent` subcommand list goes from:

```
list  start  add  remove  set-role
```

to:

```
list  start  add  remove  promote
```

This is a breaking change to the CLI surface. clank is
pre-1.0 (no compat guarantee) and `set-role` is used only
inside this repo. **PINNED (2026-06-06): clean break.**
No deprecation alias. Rationale: the soft-landing option
would have to do `set-role <X> reviewer` → ERROR
(demote-only-now-unsupported) anyway, so a third of the
old surface dies regardless. Cleaner to remove the whole
verb in one cycle than to preserve a partial alias that
itself errors on half its inputs.

## Verified before promotion (2026-06-06)

- `set-role` exists at `agent.rs:589-590`, signature
  matches the plan's claim.
- Multi-master diagnostic at `open_zellij.rs:117-121`
  surfaces exactly the `set-role <label> reviewer`
  string. Test asserting that diagnostic at
  `open_zellij_integration.rs:251` needs the suggested
  string updated too.
- Existing test
  `clank_agent_set_role_flips_role_in_place` at
  `agent_add_remove_integration.rs:443` needs migrating
  to `clank_agent_promote_flips_role_in_place` (or
  similar). Today's body just does set-role and asserts
  the role flipped — same semantic under promote.
- **Confirmed Out-of-scope follow-up is real**:
  `clank agent add --role master` at `agent.rs:446` does
  NOT check for an existing master. Adding a second
  master via `add` is currently possible. Same invariant
  violation as `set-role <X> master`. This plan does NOT
  fix that path; a follow-up should make `agent add
  --role master` invoke the same atomic-promote logic
  (or delegate to it).

## Edge cases

- **No agents declared yet**: `promote <X>` errors with
  the existing "no repo-scope `agents` declaration"
  message — same as `set_role` does today. No new code
  path.
- **`<label>` doesn't exist**: same error as `set_role`
  today — "agent `<X>` not in repo-scope `agents`".
- **Atomicity**: the write is one
  `write_repo_config`/`write_user_config` call after both
  in-memory mutations. No risk of mid-flight crash leaving
  a two-master state — that's the entire point.
- **`--global` semantics**: same as `set_role` —
  promote in `default_agents` (user scope) vs `agents`
  (repo scope). Both supported. A user-scope master and a
  repo-scope master can coexist (the repo entry shadows);
  promotion happens in the scope you target.

## Out of scope

- A `demote` command. Symmetric to `promote` but
  produces the zero-master state that breaks
  `clank open zellij`. If we ever support
  reviewers-only mode, demote becomes meaningful — until
  then it'd be a new foot-gun.
- A `swap <a> <b>` command. `promote` already handles the
  swap implicitly; an explicit swap is just sugar with
  no extra power.
- Renaming `agent add --role master` to something else.
  `add` declares a new agent; "promote on add" is a valid
  semantic that `add` already gets right (and rejects if
  the role would create two masters? — verify; if not, a
  separate plan can add that guard).

## Why this matters

This is an architectural fix, not a rename. The current
`set-role` shape encodes "role is an independent property
of each agent" — but the actual model is "the repo has
ONE master, and possibly N reviewers; promotion changes
who holds the master slot." `promote` makes the model
explicit in the CLI; `set-role` lies about the model and
defers the violation to a later command that has no
context for diagnosing it.
