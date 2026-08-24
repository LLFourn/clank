# an-attended-wait-is-something-you-can-open

## Why

Once the status TUI shows what an agent is waiting on
(`the-hourglass-names-what-it-waits-on`), the obvious next question is
the one it cannot answer: what IS that process, and how do I stop it?
Today the answer is to leave the TUI, find the pid by hand, and kill it
from a shell — for a wait the TUI is already displaying.

`the-hourglass-names-what-it-waits-on` has landed, so the ground this
builds on exists: the wait renders on its own line beneath its agent
(`ATTENDING_INDENT`, render.rs:507-514), emitted with `selected: false`
and a comment saying reaching it with the cursor is this plan. The
record carries `subject()`, `marker_fields()` and the pid, and
`fit_marker` already decides what fits.

## The modeling change

The agent panel's cursor indexes AGENTS. `sel` is a position in a
virtual list of `[agents…, add button]` — the agent row asks
`mode.selected() == Some(i)` for its own index (render.rs:502) and the
add button asks for `Some(snap.agents.len())` (render.rs:519). The wait
line is already emitted between them with `selected: false`, so the
rows on screen no longer correspond 1:1 to cursor positions; only the
selectable ones do.

An attended wait is a second kind of selectable thing, so that model
stops holding. The panel must walk a list of ROWS, each knowing what it
is — an agent, an agent's attended wait, or the add button — and the
cursor must move over rows while actions dispatch on the row's kind.

Retrofitting this as "agent index, plus a flag for whether the child is
selected" is the shape to avoid: it re-derives the row list at every
call site and drifts the moment a third row kind appears.

**Selection survives refresh by IDENTITY, not index.** The panel
refreshes under the cursor, and this repo has already paid for index
trust once — the swap picker had to rebind both ends by label because a
roster edit between frames would otherwise act on a pair the user never
chose.

Agent label plus task is NOT that identity. The same agent can attend
the same task again: a replacement record carries a new pid,
description and timestamp, and keying on label+task would leave an open
page — or a pending confirm — silently retargeted at a different wait
(codex on 6c170ec). The identity is the attendance INSTANCE: label,
task, AND the record's `at`, which is set per record and already
distinguishes one attendance from its successor.

When the instance is gone the row is gone; the cursor falls back to
that agent, then to the panel. A replacement is a different row, not
the same row updated.

## Behavior

Enter on an attended row opens a detail page for the WAIT, not the
agent. It shows what can actually be established:

- the description and task id
- the pid, and whether that process is alive
- how long it has been attended
- whatever process detail is cheaply available for a live pid

State plainly what is not known rather than blanking a field: a record
with no pid cannot report liveness, and saying so is the honest answer
that "cannot check" is not "ended".

## Two identities, not one

A killable attendance needs both, and conflating them is how a kill
hits the wrong process:

- the **attendance instance** — label, task, `at` — which says WHICH
  wait the user selected;
- the **process identity** — pid AND the birth stamp of the process
  that held it — which says WHICH PROCESS that wait meant.

`kill(pid, 0)` proves only that SOME process holds that pid. Pids are
reused: once the attended process exits, the OS is free to hand its
number to something unrelated, which then reads as both alive and
killable. That is a signal delivered to a stranger, and the current
liveness check cannot tell the difference.

So record a process TOKEN beside the pid when attendance is created,
and treat it as the process's identity for the life of the record.

The token must stay strong for as long as the record can persist, which
is across reboots — so a coarse birth stamp is not enough (codex on
a326329):

- **macOS**: the FULL start timeval from `proc_pidinfo` /
  `proc_bsdinfo` — `pbi_start_tvsec` AND `pbi_start_tvusec`. Seconds
  alone are not a unique birth: two processes can be born in the same
  second, and after pid reuse that is exactly when a stale record still
  matches. Both fields are present in the `libc` version this crate
  already depends on.
- **Linux**: field 22 of `/proc/<pid>/stat` is ticks since BOOT, so it
  repeats every boot — a record written before a restart can match an
  unrelated process afterwards holding the same pid at the same tick.
  Combine it with a boot identity: `/proc/sys/kernel/random/boot_id`,
  or an equivalently stable per-boot value.

Make it a TYPED, VERSIONED token rather than a bare number. A record
whose token version is unknown — written by a newer clank, or by an
older one that stored something weaker — must be refused outright
rather than compared field by field against a shape it does not have.
Guessing at an unrecognised identity is the failure this whole section
exists to prevent.

Both places that consult it compare the COMPLETE token: page
eligibility, and the revalidation immediately before signalling.
Comparing a prefix is the same bug in a smaller form.

Records without a token — every record written before this plan — stay
VIEWABLE but are not killable. Refusing to signal a process we cannot
identify is the same principle as "cannot check is not ended": the
honest answer to an unanswerable question is to say so, not guess.

## Killing

The page offers a kill, and it is the one destructive action here:

- Offered ONLY when the record carries pid AND birth stamp, and the
  process live at that pid still matches it. Otherwise the page says
  which of those is missing rather than showing a control that cannot
  safely work.
- Behind a confirm, like the other destructive actions in this TUI.
- **Re-resolve and re-validate immediately before signalling**, after
  the confirm. Everything checked when the page opened is stale by
  then: the record can be replaced, the process can exit, the pid can
  be reused, all while the confirm sits on screen. If the instance or
  the process identity no longer matches what the user confirmed,
  signal NOTHING and say why.
- **SIGTERM to the recorded pid alone — not the process group.** The
  birth stamp identifies one process; nothing here has verified what
  else shares its group, and signalling an unverified group is the same
  class of mistake as signalling a reused pid, one level up. The
  consequence is real and must be stated in the confirm text rather
  than papered over: children of that process may survive it. Killing a
  tree is a separate question and needs its own verification.
- The kill does NOT delete the attending record. Reaping belongs to the
  hook; a killed process becomes a dead pid, which the existing
  liveness rendering already handles by drawing nothing.

## Required tests

Selection and rows:

- The row list is walked as rows: a panel with an attending agent has
  one more selectable row than agents+1, and the cursor reaches it.
- Enter on an attended row opens the wait's page, not the agent's.
- Selection survives a refresh that REORDERS the roster — the same
  instance stays selected, asserted against the stale index.
- An attended row whose agent leaves the roster falls back without
  selecting something the user did not choose.

Identity, and all of these must signal NOTHING:

- The record is REPLACED by one with the same agent and task but a new
  `at` and pid, under an open page. The page must not retarget.
- The recorded pid is alive but its token does not match — the reuse
  case. The wait may still be shown; kill must be refused.
- The token matches on every component EXCEPT the sub-second one
  (macOS `pbi_start_tvusec`). Refused: a same-second birth is precisely
  when a coarse stamp lets a reused pid through.
- The token matches its start ticks but NOT its boot identity (Linux).
  Refused: ticks repeat every boot, so this is the reboot case a
  tick-only stamp cannot see.
- The token's version is unrecognised. Refused without field-wise
  comparison.
- The record carries no token at all (written before this plan).
  Viewable, not killable.
- The target changes BETWEEN the confirm and the action. Assert each of
  the three separately — record replaced, process exited, pid reused —
  because a check that only runs when the page opens passes all three
  while signalling the wrong thing.

Killing:

- Kill is absent without a pid, and the page says why.
- Kill is absent for a dead pid.
- Cancelling the confirm signals nothing.
- A confirmed, still-valid kill signals exactly the recorded target and
  nothing else — asserted at a seam. No test may signal a real process
  it did not create, none may spawn an agent binary or zellij, and none
  may depend on a fabricated pid being absent from the machine.
- The attending record survives a kill.

## Out of scope

- Reaping policy.
- Attending records for agents other than the one that wrote them.
- Any pane or session action. Panes have one owner, the reconciler.
