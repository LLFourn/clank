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

**Make the roster an input the wait watches, not a value it captured.**
The wait already watches the repo for work (`repo_watch` wakes on gate
signal dirs and the gitdir). Extend that to the roster inputs — the
repo config, and whatever else `try_resolve_via_team` reads — and EXIT
when they change.

Exit, do not re-resolve in place. Exiting is the smaller change and the
honest one: the caller decides what to arm next, the in-flight slot is
freed, and the next idle re-arms with a freshly resolved role. A wait
that silently switched its own role would be a second place where role
is decided.

**And do not arm a wait on a FAILED resolution.** The hook currently
launders failure into a confident answer:

    let role = resolve_role(&repo, &label).unwrap_or_else(|_| Role::default());

whose comment claims *"Reviewer is the conservative default"*. It is
not. For an agent that is master, Reviewer arms a long-poll that can
never return and — with one in-flight slot — permanently wedges the
session. That is the opposite of the comment's stated goal that "a
misconfigured repo shouldn't block the agent's session". The
conservative response to a failed resolution is to arm NOTHING and let
the next idle retry, which self-heals the moment the roster resolves.

Decide in review whether that becomes a Diagnostic (visible) or a quiet
Silent (invisible but harmless). Prefer visible: this failure was
undiagnosable from the outside for hours.

## Required tests

- A wait whose roster input changes under it exits, rather than
  continuing to block on the old role.
- A wait exits on a roster change even when it has NO work — the
  wedged case is precisely the one with nothing to return.
- Role resolution failing produces NO armed wait, and is distinguishable
  from "resolved, and there is no work".
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
