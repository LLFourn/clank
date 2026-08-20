# a-dead-pid-voids-a-falsely-live-marker

## Why

`attending` now silences EVERYTHING while its task is live, so the
liveness signal is the only thing bounding the silence. That signal is
`live_task_ids()`, read from claude's `background_tasks`, and it has
been observed reporting work that had already finished.

Directly observed this session: the hook's hint named `bm2g5c5y7,
blf4eithy, b5duj1feu` as live background tasks after all three had
already delivered completion notifications.

Then confirmed with a pid attached, which makes it unambiguous. Task
`bctmopf2c` was a `cargo install` that exited 0; its process, pid
98707, was gone from the process table; and the very next Stop hook
still announced it as a live background task. There is no reading of
that in which the task was running.

That contradicts one of the two assumptions the code rests on —
`hook_io.rs:58` states claude lists ONLY live tasks (`[]` once they
finish), and `live_task_ids` additionally filters
`status != "completed"` as belt-and-braces. Either the field retains
finished tasks, or their `status` is a string other than `"completed"`
and the filter never matches. Which one is unknown: the raw payload is
not captured anywhere, and this plan does NOT add instrumentation to
find out.

It does not need to. The consequence is what matters, and under
blanket suppression it is severe: a task id that stays falsely live is
a marker that never gets reaped, and an agent that is silent
permanently. A missed wake is the failure this whole subsystem is
biased against, and the previous plan removed the selectivity that
used to limit the blast radius.

## Approach

Silence requires BOTH signals to agree, whenever both exist:

    live = task ∈ live_ids  &&  pid.map_or(true, pid_is_alive)

A marker carrying a pid whose process is gone is void, and is reaped,
no matter what `background_tasks` claims. A marker with no pid behaves
exactly as it does today.

This is deliberately a CONJUNCTION, not a fallback or a replacement.
The consequences are what make it safe:

- **It can only shorten silence, never extend it.** Every input that
  is silent after this change was silent before it. So it cannot
  introduce a missed wake, which is the only failure mode worth
  fearing here.
- **The pid cannot wedge the hook silent.** An agent passing a pid
  that never dies — `1`, or its own parent — gains nothing, because
  the task id must still agree. That hazard would have been real had
  the pid been made authoritative on its own.
- **It needs no knowledge of the harness's status vocabulary.** The
  OS answer is independent of whatever `background_tasks` reports, so
  the bound holds however that field misbehaves.

The pid stays DISPLAY-authoritative in the sense the previous plan
established — nothing here makes a pid sufficient for silence, only
necessary when present.

## What it does not reach

A falsely-live task id AND a live pid still silences, so a recycled
pid can still mask an ended task. The conjunction removes the
SINGLE-cause failures — a harness that lies on its own, a process that
ends without the harness noticing — and claiming more than that would
be overclaiming. Closing the remaining case needs process identity,
not process existence (start-time comparison), which costs a
subprocess or a new dependency and is not worth it for a window this
narrow.

## What this deliberately does NOT do

- **It does not make attending work on codex.** Codex omits
  `background_tasks`, so `live_ids` is always empty and the
  conjunction is always false there, exactly as today. Making the pid
  sufficient would fix codex and reintroduce the wedge hazard in the
  same stroke; that trade deserves its own plan and its own argument,
  and there is no reported codex need.
- **It does not diagnose the harness.** Capturing a real payload to
  learn whether `status` is absent or differently spelled is a
  separate, instrumentation-shaped task. This change is correct
  either way.

## Required tests

- A live task id with a DEAD pid wakes, and the marker is reaped —
  the falsely-live case this exists for, and the one reproduced with
  `bctmopf2c` / pid 98707 above.
- A live task id with a LIVE pid stays silent, for every reason, as
  today.
- A live task id with NO pid stays silent — markers predating `--pid`
  are unaffected.
- A task id absent from `live_ids` still wakes and still reaps, pid
  alive or not: the task id remains necessary.
- Suppression is unchanged for every combination in which it was
  already silent — assert this as the "can only shorten" property,
  since it is the safety argument.
- No test spawns zellij, an agent binary, or the stop hook, and none
  shells out to `ps`/`kill`.
