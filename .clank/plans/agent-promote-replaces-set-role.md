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
  - **Shared helper** (codex a71bd8e catch — see "Why this
    matters" for the invariant argument):
    ```rust
    fn ensure_unique_master(
        agents: &mut [DefaultAgent],
        new_master: &AgentLabel,
    ) -> Vec<AgentLabel>;
    ```
    Sets `new_master`'s role to Master, sets every OTHER
    entry with role=Master to Reviewer, returns the
    labels that were demoted (for diagnostic output). One
    sweep over the slice. No I/O.
  - Add `fn promote(args: AgentPromoteArgs)` next to the
    existing `set_role` (~line 590). Loads config in the
    targeted scope, finds `<label>` (error if absent —
    reuse `set_role`'s not-found error path), calls
    `ensure_unique_master`, writes back. Same
    `--global` / `--repo` flag handling as `set_role`.
  - **Update `fn add`** (codex a71bd8e catch — the same
    invariant must hold on add): when the new entry's
    role is Master, call `ensure_unique_master` on the
    in-memory agents list BEFORE writing. Same diagnostic
    output for any demotions. `add --role reviewer`
    unchanged (no master-uniqueness implication).
  - On no-op (already the only master):
    `note: \`{label}\` is already the only master in {scope}`
    to stderr, exit 0. (promote-only — add can't no-op
    since it's introducing a new label.)
  - On promotion (zero prior masters):
    `promoted \`{label}\` to master in {scope}`.
  - On promotion (1+ prior masters, repair case included):
    `promoted \`{label}\` to master in {scope} (demoted \`p1\`, \`p2\`, ...)`
    — backticks around each demoted label, comma-separated,
    no trailing "and."
  - On `add --role master` with existing masters: same
    `(demoted ...)` suffix appended to the existing
    `registered \`{label}\` in {scope} \`agents\``
    message.
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

`agent add --role master` tests (codex a71bd8e catch —
the same invariant must hold here):

- `clank_agent_add_role_master_with_no_existing_master`:
  Pre-state: reviewer=A. `add B --role master`. Post:
  reviewer=A, master=B. Stdout: `registered \`B\` in
  ... \`agents\`` only (no demoted suffix).
- `clank_agent_add_role_master_demotes_existing_master`:
  Pre-state: master=A. `add B --role master`. Post:
  reviewer=A, master=B. Stdout includes
  `(demoted \`A\`)`.
- **`clank_agent_add_role_master_repairs_pre_existing_multi_master`**:
  Pre-state: master=A, master=B (created by hand-editing
  the JSON to simulate the buggy state). `add C --role
  master`. Post: reviewer=A, reviewer=B, master=C.
  Stdout includes `(demoted \`A\`, \`B\`)`. Same repair
  semantic as promote's repair test.
- `clank_agent_add_role_reviewer_does_not_demote_master`
  (regression guard): Pre-state: master=A. `add B --role
  reviewer`. Post: master=A, reviewer=B. The shared
  helper only fires when the new entry's role IS master.

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
- **`clank agent add --role master` at `agent.rs:446`** also
  fails to check for an existing master today — same
  invariant violation as `set-role <X> master`. **Updated
  per codex a71bd8e catch**: this plan now covers BOTH
  paths via the `ensure_unique_master` shared helper.
  Original out-of-scope carve-out contradicted the plan's
  "unreachable via CLI" claim; widening the plan resolves
  the inconsistency.

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
- (removed: the `agent add --role master` carve-out is
  now in scope per codex a71bd8e — both promote AND add
  use the `ensure_unique_master` helper. The original
  carve-out contradicted the invariant claim below.)

## Why this matters

This is an architectural fix, not a rename. The current
`set-role` shape encodes "role is an independent property
of each agent" — but the actual model is "the repo has
ONE master, and possibly N reviewers; promotion changes
who holds the master slot." `promote` makes the model
explicit in the CLI; `set-role` lies about the model and
defers the violation to a later command that has no
context for diagnosing it.

**The invariant is unrepresentable via the CLI only if
BOTH role-mutation paths enforce it** (codex a71bd8e
catch). Today, both `set-role` and `add --role master`
violate it. This plan removes `set-role`, replaces it
with `promote` (which enforces unique-master), AND has
`add` enforce the same invariant on creation. After this
plan, no CLI command can produce a two-master state
regardless of starting state. The shared
`ensure_unique_master` helper centralizes the invariant
in one place rather than duplicating the check across
two code paths — which is how invariant drift starts.
