# a-reviewer-pane-lands-where-it-belongs

Spawning a reviewer into an existing tab shows the pane in the wrong
place — below the status pane — and then moves it into the reviewer
stack a moment later. The end state is right; the visible jump is not.

## It is two zellij actions, and it was meant to be one

`add_reviewer_pane` focuses an anchor, runs `new-pane`, and returns.
Stacking happens afterwards in `stack_reviewer_panes`, a separate
`stack-panes` action. Between the two, zellij has already drawn the
pane wherever its default placement put it.

The intended design is written down but not implemented.
`find_anchor_pane`'s own doc says:

> focus a current stack member, then `new-pane --stacked` joins that
> stack

`new-pane` is invoked without `--stacked`. The flag exists in the
installed zellij. So the pane is created loose and then relocated,
which is exactly what the doc describes avoiding.

## Change

When an anchor was focused, `new-pane` gets `--stacked` and the pane
is born in the stack. No move, nothing to see.

`stack_reviewer_panes` stays. It is not redundant: it reconciles panes
`--stacked` cannot place — the first reviewer in a tab with no stack to
join, a pane whose anchor could not be identified, and any arrangement
drifted by hand. Deleting it would trade a cosmetic bug for a
structural one.

## The capability question this must answer

`--stacked` is a `new-pane` flag on the INSTALLED zellij, but this
codebase already knows client capability varies: `stack-panes` is
probed with `--help` before use, because a client without it produces
no error, just a wrong arrangement.

`--stacked` deserves the same treatment or an explicit argument for
why it does not. An unsupported flag that makes `new-pane` fail
OUTRIGHT is worse than the jump this plan removes — the reviewer would
not spawn at all. Establish which failure mode an old client gives
before shipping, and fall back to the current two-step if it is not
safe.

## Tests

- With an anchor, the `new-pane` argv contains `--stacked`.
- With no anchor, it does not — there is no stack to join, and the
  flag would be a guess about layout.
- `stack_reviewer_panes` still runs, so the reconciling path is not
  quietly deleted.
- The fallback, if the probe says the flag is unsafe, produces exactly
  today's argv.

## Out of scope

- The layout geometry itself (`zellij-pane-placement-and-cost`).
- Moving panes between tabs, which zellij 0.45.0 does not expose.
