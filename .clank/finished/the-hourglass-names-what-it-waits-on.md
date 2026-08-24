# the-hourglass-names-what-it-waits-on

## Why

An attending row can render as `⌛ 2m` — waiting, for two minutes, on
nothing it will name. The hourglass announces a wait and withholds its
subject, which is the one thing a human needs to judge whether to step
in.

This is a regression from `the-attending-row-fits-and-shows-only-live-waits`,
and the reasoning that caused it looked sound. That plan dropped the
harness task id, arguing the pid was "the half a human can act on".
True when there IS a pid. But `--pid` is optional and `clank attending
<id>` alone is the common call, so `row_marker` falls to:

    (None, Some(age)) => age,
    (None, None)      => String::new(),

and the marker degrades to a bare duration, or to nothing. The
identifier was treated as the disposable half and the pid as the
identity. That is backwards: the pid is an EXTRA buying liveness and a
`ps` handle; the task is what the row is about.

The plain `clank status` line still prints the id, so the TUI — the
surface with the least room to waste — is the one saying less.

## The invariant

A rendered attendance marker always names what is attended. If there is
nothing to name, there is no marker to draw.

That is stronger than "put the id back", and it is what makes the class
uninhabitable: no representable record may produce a marker naming
nothing.

## The marker's fields

Three fields, in priority order left to right:

    ⌛ <subject> · <age> · pid <pid>
    ⌛ test run · 2m · pid 41293

- **subject** — the description if one was recorded, else the task id.
  This is the identity, and the reason the marker exists.
- **age** — how long it has been attended. Answers "is this stuck?".
- **pid** — the `ps` handle. Only matters when you are about to act,
  and acting on it is the NEXT plan's detail page.

Fields drop right to left, so a narrowing line only ever gets shorter:
pid goes first, then age. The subject never drops.

## What the subject says: two words

The task id is an opaque harness handle (`br9749ewy`), already reported
as unhelpful — "what is that id? it doesn't look like a PID". Using it
as the subject satisfies the letter of the invariant and little else.

Record a VERY short description — two words, e.g. `test run` — supplied
by the caller that knows it. The agent backgrounding `cargo test -p
clank` knows it is a test run; nothing downstream can recover that.

- `clank attending` takes it alongside the id.
- `Attending`/`Attended` carry it, OPTIONAL: records already on disk
  have none and must keep loading, with the id as subject.

Two words is a budget, not a suggestion. It belongs in the flag's help
and, if enforced, enforced by truncation rather than by rejecting the
call — a wait that fails to record because its label was wordy is worse
than a wordy label.

## Where it goes: a line of its own

Put the attendance on its OWN line, indented beneath the agent, rather
than crowding the agent row:

    ▶ claude  master
      ⌛ test run · 2m · pid 41293

The agents list is currently indented by two columns for no purpose
(`plain("  ")` in the agent-row spans). Reclaim that: de-indent the
agents and spend the indent on the child line, which is what the indent
is actually for.

The child line is NOT selectable in this plan. Rows stay 1:1 with the
cursor's agent indices; the child is emitted unselectable so the
existing `mode.selected() == Some(i)` model is untouched. Selecting an
attended wait, inspecting the process and killing it is a separate plan
and must not be started here.

## Width: the subject is the last thing standing

Once pid and age are gone and the subject alone still does not fit, it
truncates with `…` — but only down to a floor. A subject shown as `t…`
names nothing; it is the bare hourglass again wearing an ellipsis.

So: truncate the subject to at least SIX display columns of its own
text plus the marker. If even that does not fit, draw NO MARKER AT ALL.

That is the plan's invariant applied to width rather than an exception
to it — a marker that cannot name its subject is not a smaller marker,
it is the bug. A pane too narrow to say what a wait is says nothing,
and the agent row above it is still there.

`char_width` is complete now so the truncation maths can be trusted,
and `…` stays the truncation marker — it must not be borrowed for
anything else.

## Required tests

- No representable `Attended` renders a marker that names nothing.
  Assert over the cases: description+pid, description only, id+pid, id
  only, and a record with neither age nor pid.
- A description is the subject; without one the id is.
- A record written before this change (no description field) loads and
  renders its id as subject — the on-disk format stays backward
  compatible.
- The child line is indented under its agent, and the agent row is no
  longer indented.
- Fields drop right to left as the pane narrows: pid first, then age,
  and the subject is still whole when both are gone.
- Below that, the subject truncates with `…` and never below the
  six-column floor.
- Narrower still, NO marker is drawn — assert specifically that a bare
  `⌛`, and an `⌛ …` naming nothing, are both absent. This is the
  invariant's width case and the exact shape of the reported bug.
- A dead pid still renders nothing at all — the liveness behaviour of
  the previous plan is unchanged.
- The child line is not selectable: the cursor still walks agents and
  the add button, and nothing else.
- The plain `clank status` line keeps its current content.

## Out of scope

- Selecting an attended wait, its detail page, and killing the
  process. Separate plan.
- Reaping. The hook owns it.
- Making `--pid` mandatory. Recording a wait stays a one-liner; the
  point is that the marker never needs the pid to be meaningful.
