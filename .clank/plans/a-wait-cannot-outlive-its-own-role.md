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

**A blocked wait must not be able to outlive the configuration it was
built from.**

Today the wait is a long-lived process holding an unrevalidated
snapshot, which is the same false model as a stale marker: a fact
asserted once and trusted indefinitely. The `--die-with-owner` bound
already exists for the OWNER dying; there is no equivalent bound for
the ANSWER changing.

## Approach

**The watcher already does its half.** `config.json` is in
`CLANK_WAKE_DIRS` (`crates/cli/src/repo_watch.rs:34`), so a roster
write ALREADY wakes the wait's loop today. Do not build watch plumbing
— none is missing.

The gap is what the loop does on that wake: it REFOLDS state and keeps
its fixed role, so it re-answers the same wrong question and blocks
again. An explicit `--role` is fixed input that no refold re-resolves;
omitting the flag re-resolves per fold, which is why only the
hook-armed explicit form can wedge.

So: **on a wake whose input is a roster input, exit instead of
refolding.**

**The exit contract.** Exit in the NO-WORK shape — exit 0, empty items
— so the hook maps it to `Silent{NoWork}`, the in-flight slot frees,
and the next idle re-arms with a freshly resolved role. Any other shape
is worse: a non-zero exit reads as genuine failure, and a synthesised
item reads as phantom work the agent cannot act on.

Exit, do not re-resolve in place. A wait that changed its own role
would be a second place where role is decided, and the caller is
already the place that decides it.

**Do not arm on a FAILED resolution — and say so.** The hook launders
failure into a confident answer in TWO places, both of which this must
cover:

    stop_hook.rs:147  (the armed wait)
    stop_hook.rs:292  (the peek path)

    let role = resolve_role(&repo, &label).unwrap_or_else(|_| Role::default());

Emit a **Diagnostic**, not a quiet Silent. This failure cost hours of
dead session that was undiagnosable from outside; arming nothing is
only half the fix, and a Silent that hides the reason rebuilds the
observability hole the bug lived in.

The comment above it inverts, because it currently asserts the
opposite of what happens:

    default to Reviewer — a misconfigured repo shouldn't block the
    agent's session, and Reviewer is the conservative default

Reviewer is the one default that CAN wedge the session: for an agent
that is master it arms a poll that can never return, and with one
in-flight slot nothing can replace it.

## Required tests

- A wait whose roster input changes under it EXITS, rather than
  refolding and blocking on the old role again.
- It exits in the no-work shape (exit 0, empty items), so the hook
  maps it to `Silent{NoWork}` — asserted on the shape, since a
  non-zero exit or a synthesised item would each fail differently.
- A wait exits on a roster change even when it has NO work — the
  wedged case is precisely the one with nothing to return.
- Role resolution failing produces NO armed wait and a Diagnostic
  naming the failure, distinguishable from "resolved, and there is no
  work".
- BOTH fallback sites are covered — the armed wait (`:147`) and the
  peek path (`:292`). A fix to one leaves the other laundering.
- A roster change that does NOT affect this agent's role still exits;
  correctness first, and re-arming is cheap.
- The existing `--die-with-owner` bound is unaffected — the two bounds
  are independent and neither replaces the other.
- No test spawns zellij, an agent binary, or a real editor session.

## Out of scope

- opencode's one-in-flight-slot rule. It converts this bug from noisy
  to fatal, but a wait that ends when its premise changes is correct
  regardless, and the plugin's discipline exists for its own good
  reasons (an unconditional nudge would idle-loop the session).
- Why THIS repo resolved no master at 13:19:27. Interesting, unproven,
  and the fix must not depend on it.
