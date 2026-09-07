# a-dirty-plan-is-drafting-not-committing

> Sometimes a master's status is "committing…" — but how does clank
> know it is committing? Isn't it just working? — lloyd

It is just working. `WaitingOn::MasterToCommit` is derived from one
fact on disk: the plan's `.md` has uncommitted edits
(`PlanWorktreeStatus::BodyDirty`, `crates/core/src/wait.rs`). That
state dominates the gate on purpose — a dirty plan body is author
intent that supersedes the last committed version, and reviewers must
not be woken to a doc about to change — so it holds from the first
keystroke in the plan file until the commit, however long the master
spends writing code in between.

`verb_of` (`status_tui/derive.rs`) renders it as "committing":
present-progressive, as if clank had seen a `git commit` begin. It has
no such signal. Every other verb in that table names the STATE the
master is in — "revising" (changes requested), "working" (continued),
"finalizing" (finished and clean) — and this one pretends to name an
action.

## The change

`MasterToCommit` renders as **"drafting"**: what a dirty plan body is,
and honest that the master may still be mid-thought. The two doc
comments in `scroll.rs` that list the verbs follow. No state-machine
change: the dominance rule is right and stays.

## Tests

The verb table has no test pinning "committing"; one is added for
"drafting" so the word is a decision and not an accident.

## Out of scope

- Changing when `MasterToCommit` is reached.
