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

Behavior (PINNED per codex d1c1de5 catch — pre-existing
multi-master configs must be repaired by promote, since
they're already possible via today's `set-role` /
`agent add --role master`):

1. Find all current masters in the targeted scope.
2. If `<label>` is master AND no other agent is master →
   no-op, exit 0 with
   `note: <label> is already the only master`.
3. Otherwise: atomically set `<label>` to master AND
   demote EVERY OTHER master in the same scope to
   reviewer. One write, all changes.

This guarantees the post-condition "exactly one master"
regardless of whether the pre-state had zero, one, or
many masters. The illegitimate states (multiple masters;
zero masters via demotion) become unreachable via the
CLI even if they exist on disk from earlier buggy
operations — `promote` is the repair path.

Output messages:
- No-op: `note: \`<label>\` is already the only master in <scope>`.
- Promote from zero masters:
  `promoted \`<label>\` to master in <scope>`.
- Promote with one demotion:
  `promoted \`<label>\` to master in <scope> (demoted \`<prev>\`)`.
- Promote with multiple demotions (repair case):
  `promoted \`<label>\` to master in <scope> (demoted \`<prev1>\`, \`<prev2>\`, ...)`.

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
  - Logic: load config in the targeted scope, find
    `<label>` (error if absent — reuse `set_role`'s
    not-found error path), collect labels of ALL OTHER
    entries with `role == Master`. If `<label>` is master
    AND that collection is empty → no-op. Else: set
    `<label>` to master AND set every entry in the
    collected list to reviewer. One write at the end.
  - Same `--global` / `--repo` flag handling as `set_role`.
  - On no-op (already the only master):
    `note: \`{label}\` is already the only master in {scope}`
    to stderr, exit 0.
  - On promotion (zero prior masters):
    `promoted \`{label}\` to master in {scope}`.
  - On promotion (1+ prior masters, repair case included):
    `promoted \`{label}\` to master in {scope} (demoted \`p1\`, \`p2\`, ...)`
    — backticks around each demoted label, comma-separated,
    no trailing "and."
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
    > make exactly one of them master (all OTHERS
    > automatically become reviewers)."
    Also update the assertion at
    `open_zellij.rs:257-258` ("diagnostic should suggest
    set-role") and the integration test at
    `open_zellij_integration.rs:251` to match.
- `crates/cli/tests/agent_add_remove_integration.rs`,
  `crates/cli/tests/open_zellij_integration.rs`: any
  invocation of `clank agent set-role` becomes
  `clank agent promote`. The semantics map cleanly because
  every existing test that uses `set-role` is either
  promoting a reviewer to master or doing a no-op.

## Tests

Integration tests in
`crates/cli/tests/agent_add_remove_integration.rs`:

- `clank_agent_promote_flips_reviewer_to_master`
  (rename of `clank_agent_set_role_flips_role_in_place`):
  Pre-state: master=A, reviewer=B. `promote B`. Post-state:
  reviewer=A, master=B. Asserts stdout has
  `promoted` and `demoted` keywords with the right labels.
- `clank_agent_promote_no_op_when_already_only_master`:
  Pre-state: master=A. `promote A`. Asserts no write
  (file mtime unchanged) AND stderr contains
  `already the only master`.
- `clank_agent_promote_from_zero_masters`:
  Pre-state: reviewer=A, reviewer=B. `promote A`.
  Post-state: master=A, reviewer=B. Stdout has
  `promoted` but NOT `demoted` (no prior master to list).
- **`clank_agent_promote_repairs_pre_existing_multi_master`**
  (codex d1c1de5 catch — the repair case): Pre-state:
  master=A, master=B, master=C, reviewer=D. `promote D`.
  Post-state: reviewer=A, reviewer=B, reviewer=C,
  master=D. Stdout lists ALL THREE demoted:
  `(demoted \`A\`, \`B\`, \`C\`)`. Pin: assert each label
  appears in the demoted list AND that the post-state
  has exactly one master. This is the test that defends
  the repair semantic — without it, the implementation
  could "find the first master, demote it" and still
  leave two masters, satisfying earlier acceptance.
- `clank_agent_promote_errors_when_label_absent`:
  reuse `set_role`'s existing not-found error path.
- `clank_agent_promote_atomic_write`: verify the underlying
  file is written exactly once (mtime increments once),
  not once per demote. Lock in the "one write, all
  changes" property.
- Delete `clank_agent_set_role_*` tests for paths that
  no longer exist (the demote case if there was one;
  the duplicate-master-allowed case).

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
