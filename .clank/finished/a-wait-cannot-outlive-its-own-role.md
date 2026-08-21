# a-wait-cannot-outlive-its-own-role

## Why

A `clank wait` resolves its author and ROLE once, at arm time, then
blocks indefinitely. Nothing revalidates that snapshot. If the role was
wrong when it was taken — or becomes wrong afterwards — the wait blocks
on a question that has no answer, forever, and the agent it belongs to
never wakes again.

Observed live. In `/Users/llfourn/quarantine/arrayref-2026-08-20`,
`kimi` is master:

    clank export  →  "kimi": { "tool": "opencode", "role": "master" }

but its in-flight wait, armed 13:19:27 and still blocked 2h32m later,
says:

    clank wait --repo … --author kimi --role reviewer --die-with-owner --json

Peeking both roles against that repo, right now:

    --role master    → one item: malware-reconstruction, address_commit_changes
    --role reviewer  → []

So there is master work waiting and a wait that structurally cannot
return it. All three agents' waits in that repo carry `--role
reviewer`; for `claude` and `codex` (commit tier) that is coincidentally
correct, which is why only the master's is visibly broken.

The likely trigger: the repo is a COPY, its directory stamped 12:46:44
and its `.clank/config.json` written 13:23:44 — the wait was armed
BETWEEN those, against a half-assembled repo with no master yet. That
trigger is unproven and does not matter. Any cause that yields a wrong
role at arm time produces the same permanent wedge, so the fix must
not depend on identifying it.

## What makes it permanent rather than transient

Two things, and the second is why the human has to intervene.

**The wait never re-reads the roster.** It holds a value resolved once
and blocks on it. A config write four minutes later cannot be noticed.

**The tool's loop has ONE in-flight slot.** opencode's plugin (`~/.config/
opencode/plugin/clank.js`) states it: *"At most ONE in-flight stop-hook
wait per session; an idle firing while a wait is pending is ignored."*
So a wait that never returns does not merely fail — it consumes the slot
that a corrected wait would need. Every subsequent idle is discarded.
The session is dead until a human prompts it.

That is the reported symptom: "sometimes I have to prompt it when it's
master."

## The invariant

**Role is not orthogonal to identity — it is DERIVED from it.**

Which side of the workflow an agent plays is decided by who it is: the
roster names one master, and everyone else is a reviewer. A `--role`
flag makes that a second, independent input, and two sources of truth
for one fact can disagree. This wedge is what disagreement looks like.

## Approach: delete the flag

Not "detect the divergence and recover from it" — make the divergence
UNREPRESENTABLE. `clank wait` loses `--role` entirely (the flag, the
`WaitRole` enum, its conversion), and `derive_wait_inputs` resolves the
role from the author's roster entry, every refold.

The recovery machinery this plan first proposed is then unnecessary,
and so is the flag's own defaulting: its doc conceded the derivation
already — *"defaults to `master` iff the resolved label matches
`.clank/config.json`'s `master` field, else `reviewer`"* — so the flag
existed only to OVERRIDE identity, which is the bug and not a feature.

**The self-correcting path already existed and was already tested.**
`wait_config_reload` pins that an omitted role re-resolves on a wake
and returns master work after a mid-wait promotion, without the wait
restarting. The hook opted out of that by passing an explicit role.
Removing the flag makes every caller take the tested path.

**What must keep working, and does:**

- `clank wait --repo <path> --author <label>` from OUTSIDE the repo.
  Role derives from the roster, which needs neither a session nor a
  cwd inside the repo.
- `clank wait --for <event>`, which needs no identity at all. It
  already returns before author resolution, so it is untouched.

**Arming still refuses on an unresolvable role.** A role that cannot be
derived is not a role to guess, so the hook emits a Diagnostic and arms
nothing rather than defaulting to Reviewer. Resolution moved INSIDE the
auto-on arm: an agent with auto off asked not to be driven, and
diagnosing its roster would be noise about work it will not do.

The SessionStart peek needs no such guard once the flag is gone — the
peek subprocess derives its own role from identity, so it cannot be
told a wrong one. It resolves a role only to PHRASE items, and only
once there are some.

## What this deletes

- `explicit_role_survives_a_mid_wait_promotion`, which pinned the
  semantics of the removed flag. Deleted, not adapted: the behaviour
  that survives — an omitted role re-resolving mid-wait — is pinned by
  the test directly above it.
- The hook's `Role::default()` fallbacks, both of them.

## Required tests

- A parked wait whose agent is promoted mid-wait returns MASTER work
  on the wake, without restarting. This is the existing test, and it
  now covers every caller rather than only the flagless ones.
- Role resolution failing produces NO armed wait and a Diagnostic
  naming the failure.
- Auto-off outranks an unresolvable role: an agent that asked not to
  be driven is silent, not diagnosed.
- `--for` still runs with no identity resolvable at all.
- `--repo` + `--author` still works from outside the repo.
- No test spawns zellij or an agent binary.

## Out of scope

- opencode's one-in-flight-slot rule. It converts this bug from noisy
  to fatal, but a wait that ends when its premise changes is correct
  regardless, and the plugin's discipline exists for its own good
  reasons (an unconditional nudge would idle-loop the session).
- Why THIS repo resolved no master at 13:19:27. Interesting, unproven,
  and the fix must not depend on it.
