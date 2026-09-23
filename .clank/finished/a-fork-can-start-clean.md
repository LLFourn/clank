# a-fork-can-start-clean

`clank fork create --fresh` — every agent starts a new session in the
worktree instead of forking the one it was in.

> "can you please create a `--fresh` flag on clank fork where it just
> creates fresh sessions for all the agents." — lloyd

## Why

Forking carries each agent's session into the worktree, which is
usually what you want: the team arrives knowing what it was doing. But
not always, and there is no way to say so. Today the only route to a
clean start is to hand-edit `fork.json` in the new worktree before the
first launch, per agent, which is a thing you have to know about clank
rather than something clank offers.

Two reasons to want it. A carried session brings the WHOLE prior
conversation with it, and when the fork is for something unrelated —
reviewing a PR, chasing a bug in another part of the tree — that
context is noise the agent has to work past. And a carried codex
session can be one the model's own server-side compaction refuses:
`invalid_encrypted_content`, because a reasoning item's encrypted
payload is bound to the id it was issued under, and forking replays it
under another. That is not clank's bug to fix, but "start clean" is
the operator's way around it, and it should be a flag.

## The build

**`--fresh` on `fork create`.** Every member's spec is seeded with no
source session, whatever its binding was — the same shape a member
with no bound session already produces, so the launch path is
unchanged: `clank agent start` sees no session to fork and creates
one, and the ORIENTATION PROMPT still rides along, because a fresh
session in a fork must not start blind.

**The report says what it did.** Seeding already reports counts and
warnings from one classification, so a `--fresh` fork reports every
member as fresh and does NOT warn about missing or stale bindings —
there is nothing to warn about when starting clean was the request.
That means the flag joins the classification, not a filter bolted
after it: the counts and the warnings must not be able to disagree.

**It says so in the help and the README.** One line each: what it does
and when you want it.

## Tests

- With `--fresh`, every member's persisted spec carries no source
  session, including members whose binding was live.
- The report counts every member as fresh and emits no
  missing-binding or stale-binding warning.
- Without it, nothing about today's behaviour changes — the existing
  classification tests still hold.
- The orientation prompt is present in a `--fresh` spec.

## Out of scope

- Per-agent selection (fresh for some, forked for others). If that is
  wanted it is a different flag with a different shape, and nobody has
  asked for it.
- Anything about the tools' own fork/resume behaviour, including the
  codex compaction failure that motivates one use of this.
