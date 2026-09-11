# nothing-refuses-in-silence

> I just tried to swap an agent for grok and it didn't work. The agent
> just has "?" in front of it and when I go to "reopen pane" nothing
> happens. Also reopen pane should return and display an error if it
> didn't work. Clearly it didn't work since it didn't even create a new
> zellij pane to even try and add the agent. The repo this is happening
> in is ~/src/penlock-experiment.
>
> Include in scope not ending up in the situation where you have two
> TUIs.
>
> If the agent doesn't get a pane I want it to die. — lloyd

## What actually happened

Not grok. `clank agent start grok --print` in that repo resolves a
real command and `grok` is on PATH; the roster is `codex` (master) +
`grok` (commit), and grok's session was bound at 2026-09-10T23:26.
The `💤 penlock-experiment` tab holds exactly two panes — `codex
(master)` and `status` — and no grok pane has ever been made, which is
why the row reads `?` (`Presence::Missing`: on the roster, no live
pane).

The reason is a lock:

    $ lsof ~/src/penlock-experiment/.clank/zellij-reconcile.lock
    clank   76361 …/penlock-experiment/.clank/zellij-reconcile.lock

    pid 96886  started Fri 11 Sep 09:25  ZELLIJ_SESSION_NAME=clank-penlock-experiment  pane 28
    pid 76361  started Wed  2 Sep 17:34  ZELLIJ_SESSION_NAME=clank-full-app-sim--38b0  pane 59

Both are `clank status --repo ~/src/penlock-experiment --tui`. The one
holding the reconcile lease has been sitting in a DIFFERENT zellij
session for nine days. The TUI in the penlock tab — the one lloyd is
looking at, the one that owns those panes — gets `may_reconcile() ==
false` on every pass and on every reopen, so `reopen` returns
`HeldElsewhere` before it touches zellij. Hence "it didn't even create
a pane to try": nothing was ever attempted.

And the answer went where nobody could read it. A reopen outcome is
mounted with `mount_notice` into `snapshot.log_rows` — but the reopen
is pressed on the agent DETAIL page, which `render_at` serves as a
dedicated full screen that never draws the log; and the notice is
cleared by the next successful refresh, which the reopen itself
provokes. The refusal was correct, phrased, and unreachable.

## The model

> One lease, held by the one TUI that owns the repo's panes. And a
> refusal reaches the hand that asked.

Two mistakes, and the second is why the first survived nine days
without a bug report naming the real cause.

- **One lease, taken at startup.** `ReconcileLease` is acquired lazily
  per pass, keyed by the repo, and asks "may I reconcile right now?".
  Scoped that way it is answered by a TUI in an unrelated session,
  which reconciles a different tab and races nobody. But the fix is
  not a better key — it is that the question belongs at the START of
  the process, once, as ownership rather than per-pass permission:

      a status TUI takes the repo's lease when it starts, holds it for
      its life, and does not start without it.

  An flock on `.clank/status-tui.lock`, with the holder's pid,
  session, pane and the RFC3339 it was taken at written into the file.
  The OS frees it when the process dies, so there is no cleanup
  protocol and no staleness (the property the old lease already
  relied on).

  A second `clank status --tui` for the same repo then does not exist
  to be serialized against: it reads the holder back and exits saying
  where the first one is — `held by pid 76361 in session
  clank-full-app-sim--38b0 since 2026-09-02` — and since clank does
  not pass `--close-on-exit`, the pane keeps that sentence on screen.
  A holder file that cannot be read or parsed still refuses, without
  attribution; it is a diagnostic, never the gate.

  It refuses rather than taking over. The newcomer is usually the one
  the user wants — but a status pane that kills another process to
  claim a repo is a worse power to own than a message telling you
  where the other one is, and the message is enough to act on. If that
  trade proves wrong the answer is a flag, not a signal.

- **So the per-pass lease goes, entirely.** With ownership settled at
  startup there is no second compliant reconciler in any session, and
  a second protocol that can no longer fire is worse than none: it
  reads as a live safeguard while proving nothing.
  `ReconcileLease`, `PaneIo::may_reconcile` and its two call sites,
  `ReopenOutcome::HeldElsewhere`, and the refused-pass state all go
  (codex on b9f17c9). What remains of "the pass did nothing" is the
  honest set: a listing that failed, a pane that was not created, a
  pane that landed wrong.

- **The outcome is shown where the action was taken.** The reopen
  answer arrives asynchronously from the worker, so it is delivered
  against the page the user is on WHEN IT ARRIVES:
  - still on that agent's detail page, and the outcome is not
    `Reopened`/`AlreadyOpen` → an error overlay, the same
    `Overlay::error` the detail page's other failures already use;
  - anywhere else, or a success → the log notice, as today.
  `NoListing`, `NotCreated` and `Unplaced` are the outcomes that
  reach a user this way — each one leaves the pane missing, which is
  the whole reason the key was pressed. A success needs no
  interruption.

- **The doctor names a command that no longer exists.** Its OK line
  reads "`stack-panes` available in the zellij client" while
  `placement_capability` probes `override-layout` (the WARN arm says
  so). Whoever debugs this next reads that line; it should name what
  was actually probed.

### An agent outlived its pane

The rest of the incident, which the lease explains and does not fix.

The rogue TUI held the lease, so IT was the one reconciling this
repo's roster — into its own session's tab, which lloyd could not see.
When `glm` joined the roster at 09:27 that TUI gave it a pane there
(pid 5329, `ZELLIJ_PANE_ID=70`, started 09:27:15, the same minute glm
armed its wait). When glm was swapped back out, the pane went with it:
session `clank-full-app-sim--38b0` has no pane 70 today.

The AGENT did not go. It kept its binding, kept its parked wait, took
the review item for `693d129` and wrote its verdict at 09:33 — from a
process whose pane had already been closed. That is the "agents ran
without panes" lloyd saw, and it is not a display problem.

> An agent lives in its pane. When the pane is gone, the agent is
> over.

The pane is the agent's whole context — it is where clank put it, what
the roster grants and revokes, and what a human closes to stop it. A
process that survives it is unreachable by every one of those and
still writing to the repo. Closing the pane is supposed to be enough;
node does not always agree.

So the agent checks, at the one moment it already runs clank code —
its own Stop hook. It knows `ZELLIJ_SESSION_NAME` and
`ZELLIJ_PANE_ID`; if its session's pane listing does not contain its
own pane, its pane is gone and it exits rather than arming anything.

Only on POSITIVE evidence: inside zellij, a listing that answered, and
its own id absent from it. Not in zellij, no pane id, no listing —
nothing is concluded and nothing is killed. A hand-started session
bound with `clank as` has no pane of clank's and must keep working;
the check is "the pane I was given is gone", never "I cannot find a
pane for me".

The decision is pure over those inputs, so it is tested without a
zellij: `pane_is_gone(session, pane_id, listing)` is true for exactly
one shape of evidence and false for every uncertainty.

### Why startup is the enforcement point

Killing the rogue TUI freed the lease immediately — and the tab STILL
did not gain its grok pane. A reconcile pass runs on exactly three
triggers: process start, a snapshot rebuild whose input signature
changed, and entering the agents screen. A quiet repo fires none of
them, so a TUI that was refused at startup stays unreconciled until
the user happens to touch something unrelated.

Startup is the one moment that is guaranteed to happen, and it is
where ownership is decidable with no ambiguity about who arrived
first. That is the moment to answer the question — and to say so out
loud, because the alternative is what this incident was: a correct
refusal, repeated for nine days, that nobody could see.

## Tests

- The lease: one holder per repo; a second acquire is refused while
  the first lives and succeeds once it is dropped (flock semantics, no
  cleanup protocol). A DIFFERENT repo is never blocked by it.
- A refused start reports the holder recorded in the file (pid,
  session, pane, taken-at) and exits nonzero; an unreadable or garbage
  holder file still refuses, with the unattributed wording; a holder
  file naming a dead pid is not consulted at all, because flock
  already granted the lease.
- Delivery: an outcome that is not `Reopened`/`AlreadyOpen` mounts an
  error overlay while the detail page for THAT agent is open; the same
  outcome with the panel focused (or a different agent's page open)
  mounts the log notice instead; `Reopened` never raises an overlay.
  One case each for `NoListing`, `NotCreated`, `Unplaced`.
- Nothing left in the reconciler asks permission: no call site of the
  deleted `may_reconcile` survives, and a pass with a working listing
  and a missing pane opens it.
- `pane_is_gone`: true only for a live session, a listing that
  answered, and an id absent from it; false for no session, no id, a
  failed listing, and an id present. The FALSE cases are the ones that
  matter — each of them is an agent that must keep working.
- An agent whose pane is gone arms nothing and ends, and one whose
  pane is present is untouched (the same hook, two evidence sets).
- Doctor: the placement-OK line names `override-layout`, and the
  string it prints is the one the capability probe asked for.

Mutation-checked with production-only edits: the startup refusal
turned into a warning that continues; the holder read removed; the
overlay downgraded to a notice.

## Out of scope

- Killing or reaping a TUI that is already running. The second one
  refuses to start; the first is left alone. Reaping a process from a
  status pane is a bigger power than this needs.
- Reaping an agent from outside itself. The check is the agent's own,
  at its own turn-end, on positive evidence about its own pane — no
  process tables, no cross-session pane closing.
- Re-triggering a reconcile pass on a timer. A quiet repo fires none
  of the three triggers, which is how the tab stayed empty even after
  the rogue TUI died — but with ownership settled at startup the only
  way to reach that state is a failed listing, and the next wake
  retries it. Worth revisiting if that turns out to be optimistic.
- Taking over a lease held by a LIVE TUI in the same session. That is
  the race the lease exists for.
- Why grok's pane never appeared BEFORE the swap, if it ever did:
  there is no pane and no evidence of one, and with the lease fixed
  the reconciler makes it on the next pass. If it then fails to start,
  that is a different failure with its own visible outcome
  (`NotCreated` / `Unplaced`).
- The `?` glyph itself: it is correct — the agent has no pane.
