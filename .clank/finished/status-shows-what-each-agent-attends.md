# status-shows-what-each-agent-attends

## Why

`clank attending <task-id>` exists so an agent waiting on a background
task stops being woken about work it is already doing. It did not do
that, and nothing showed the human whether it was in effect, so the
failure was invisible from both sides.

Reported first by a reviewer agent: it called `clank attending` three
times and every call was a no-op, returning "nothing standing to
acknowledge right now, so all work still wakes you". It had no way to
see that state except by reading the command's own transient output,
and no way at all to see it for another agent.

Then reproduced, decisively, by master in this session. Waiting on a
test run with `ready_to_finalize` standing, it called `clank attending`
seven times, got the same no-op every time, and was woken roughly
twenty times over the length of one build.

## What was actually wrong

Suppression was SELECTIVE. `is_standing_work` admitted exactly one
item shape — kind `master`, reason `gate_continue` — so anything else
standing (`ready_to_finalize`, `commit_plan_revision`) could not be
acknowledged by any marker, and the loop ran on.

Worse, the predicate existed TWICE: once in `stop_hook.rs` to decide
what to suppress, and once inline in `attending.rs` to decide what to
record. Two sources of truth for "work the agent is already doing",
which is why widening either one alone would not have fixed it.

An earlier draft of this plan called that narrowness deliberate and
put it out of scope, on the argument that suppressing `Revise` would
mean sitting through newly arrived REQUEST_CHANGES for the length of a
build. That argument does not survive contact: an agent blocked on a
build cannot act on review feedback anyway, and nothing is lost —
the item is still there, unchanged, the moment the task ends.

## The model

**A live marker silences the hook completely.** Every item, whatever
its reason. `clank attending <task-id>` means: I am waiting, wake me
for nothing until this ends.

**Safety is liveness, not selectivity.** The recorded id is validated
against the tool's live task list on every pass and the marker is
deleted the moment it goes stale, so the silence lasts exactly as long
as the work and cannot be claimed for longer. A corrupt marker fails
toward noise. This is the property that makes blanket suppression safe,
and it is the one that must never be weakened.

The record is therefore just `{task}`. `plan` and `sha` existed only
to make the narrow match work; both copies of the predicate are gone.

## The surfaces

The two are NOT symmetric, and assuming they were is what this section
originally got wrong.

**`clank status` has no agents section.** It renders repo / branch /
head / dirty / queue / fork / stashed / pr / plan; `StatusSnapshot`
carries `agents`, but only the TUI consumes it. So do not invent a
roster table here. Follow the file's existing idiom — `fork:`,
`stashed:`, `pr #N:` each emit a line only when they have something to
say — and emit one line per agent that HAS a marker, nothing when none
do:

    attending: claude → b63yj8abk

The line says what the marker now means and nothing more. There is no
acknowledgement half to render: a marker either exists, and that agent
is silenced until its task ends, or it does not.

A repo where no one is attending anything looks exactly as it does
today.

**The TUI already has an agents panel** (`render.rs`, gated on
`!snap.agents.is_empty()`). There the marker belongs on the agent's own
row, since the row already exists and the panel is where a human looks
to compare agents.

Read from `.clank/agents/<label>/attending`, the same record the hook
writes.

## What status CANNOT know, and must not imply

`background_tasks` reaches only the hook payload. A separate `clank
status` process cannot validate liveness, so it must NOT print
anything implying the task is running. It reports what is recorded.

The hook deletes a marker whose task is dead, so a marker on disk is
live-or-not-yet-reaped. Divergence between what status shows and what
is real is exactly the signal worth exposing — do not paper over it by
inventing a liveness check status cannot perform.

## Required tests

- A live marker silences EVERY reason — `ready_to_finalize`,
  `address_commit_changes`, `unblocked`, and a `gate_continue` at a
  sha the marker never saw. These are precisely the cases the old
  selective model let through.
- A stale marker (task absent from the live list) suppresses nothing
  and is deleted, not left to rot.
- A corrupt marker suppresses nothing and survives on disk — status is
  read-only and must not delete it; reaping belongs to the hook, which
  owns the liveness authority.
- A marker renders as `attending: <label> → <task>`; an absent marker
  renders as no attendance.
- The on-disk field name is pinned against what the hook writes, so a
  rename cannot silently make every marker unreadable — a failure that
  would look identical to nobody attending anything.
- No test spawns zellij, an agent binary, or the stop hook.

## Out of scope

- Whether the tool's `background_tasks` can report a task as live
  after it has ended. Observed once this session (three completed task
  ids came back as live), and it matters MORE under blanket
  suppression, since a falsely-live id now silences everything rather
  than one reason. Its own plan: the fix is in liveness reporting, not
  in narrowing suppression again.
