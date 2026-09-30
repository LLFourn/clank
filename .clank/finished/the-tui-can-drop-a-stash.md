# the-tui-can-drop-a-stash
# The TUI can drop a stash

## Why

Every other stash verb is reachable from the TUI's plan page. An active plan stashes with
`s` (`PlanAction::Stash`), a stashed one restores with `PlanAction::Restore` (`stash pop`),
and a stashed plan has a page of its own that reads its body from the protective ref.

`clank stash drop` is the exception. A stash you have decided against — superseded work, an
approach you abandoned, a plan whose premise a later plan destroyed — can only be cleared
by leaving the TUI for a shell. So the list grows, and the entries that are genuinely dead
are indistinguishable from the ones still owed.

Concretely, right now: `the-waker-is-per-turn-but-the-session-is-not` (11 commits) is
DELIBERATELY dead — it detects a dead waker from the process table so a second mechanism
can stand down, and the design it backed was removed. It will sit in that list forever
because dropping it means remembering a command.

## What makes this different from every other action on that page

**`stash drop` is the only irreversible one.** `clank stash drop` says so itself:

> dropping the stashed commits of `<plan>` — the only copy of that work; `git reflog` will
> not have them

That is not the usual git hedge. A stash's commits were rebased OFF the branch when it was
pushed, so the protective ref is the only thing holding them. Drop the ref and they are
unreachable immediately, not after gc.

Compare the neighbours: `stash push` is undone by `pop`. `unqueue` keeps the body in
`drafts/`. Even `purge --drop` leaves the commits in the reflog. This one leaves nothing.

So the confirm cannot be the same confirm. The plan has to decide what it says and what it
shows, because "are you sure?" over an irreversible delete of 11 commits the operator
cannot see is not a decision they are equipped to make.

## Questions

1. **What does the confirm show?** At minimum the commit count. Probably the subjects —
   the page already reads the stash's body and commits from the ref for `stash show`, so
   the data is in hand. Decide whether the confirm reuses the page's existing rendering or
   gets its own.
2. **Which key?** `s f c p u r v` and `o` are taken on that page. `d` is free and obvious,
   which is also the argument against it — the most destructive action should not be the
   easiest keystroke. Consider requiring the row to be activated rather than a direct
   hotkey, which is what `Purge` already does by opening a chooser instead of acting.
3. **Does it belong on the purge chooser instead?** `PlanAction::Purge` is described as
   "the danger door: opens the purge chooser (artifacts vs drop)". A stashed plan has no
   artifacts to strip, so the chooser as it stands does not fit — but a shared "destructive
   things live behind one door" shape might be better than a second door.
4. **What happens to the page afterwards?** The page's SOURCE is the stash; once dropped
   there is nothing to re-read. It must return to the panel rather than refresh into an
   empty page or, worse, silently fall through to an active plan of the same stem — the
   page comment warns that a stash and an active plan may share one.

## Acceptance

- A stashed plan can be dropped from its TUI page, and the stash is gone from
  `clank stash list` afterwards.
- The confirm names what is being destroyed — the plan and its commit count at least — and
  says it cannot be undone. A test asserts the wording, because this is the one action
  where a vague confirm is the defect.
- The action is offered ONLY on a stashed page. An active, queued or finished plan must not
  show it, with a test over the row derivation like the existing ones.
- After a drop the page leaves rather than re-reading a source that no longer exists, and
  a stash sharing a stem with an active plan does not cause the page to fall through to it.
- Mutation-checked: an offered-everywhere `Drop`, and a confirm that fires the action on
  the wrong answer, both fail a test.

## Out of scope

`clank stash drop` itself — the command is correct and this is about reaching it. The other
stash verbs. Any change to what a stash IS or how `push`/`pop` work.
