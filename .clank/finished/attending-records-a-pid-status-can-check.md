# attending-records-a-pid-status-can-check

## Why

`clank status` prints attendance it cannot verify, so it shows work
that has already finished:

    attending: claude → bqo7ncyld

Observed directly: the marker was written at 14:51 for a `cargo
install` that had since completed, nothing was running, and status
still reported the agent as attending. The human's reaction was the
correct one — "I don't see any shell running! also what is that id? it
doesn't look like a PID."

Two defects, one cause.

**The identifier is meaningless outside the agent's harness.**
`bqo7ncyld` is a Claude Code background-task id — the handle
`run_in_background` returns. It cannot be looked up, correlated with
anything the human can see, or checked by any process other than the
Stop hook, which alone receives `background_tasks`.

**Reaping is therefore lazy, and only the hook can do it.** A marker is
deleted when a wake finds its id absent from the live list. Between the
task ending and the next Stop hook there is a window — unbounded, since
a quiet agent may not Stop for a long time — in which status reports
attendance that ended.

The finished plan `status-shows-what-each-agent-attends` defended this
as "divergence between what status shows and what is real is exactly
the signal worth exposing". That is too clever for a user-facing line.
A status line that says an agent is waiting when nothing is waiting
reads as a bug, not as a signal, and it discredits the line in the one
situation it exists to report on.

## Approach

Record a PID alongside the task id. A pid is checkable by ANY process,
so `clank status` can verify liveness itself instead of being forbidden
from it.

The marker becomes:

    {"task": "bqo7ncyld", "pid": 54569, "since": "2026-08-20T14:51:09Z"}

`task` stays and stays authoritative for the HOOK: its validation keys
on `background_tasks`, which is the only thing that knows whether the
agent's own harness still considers the work live. Nothing about the
hook's suppression logic changes.

`pid` and `since` are for DISPLAY. Status renders:

    attending: claude → 54569 (bqo7ncyld) · 4m

and, when the pid is gone:

    attending: claude → 54569 (bqo7ncyld) · stale, ended

## Checking liveness from inside Rust

No subprocess. `crates/cli` already depends on `libc = "0.2"` directly
and already uses it this way (`flock` in `github_event_log.rs:300`,
`ioctl` in `term.rs:27`), so this needs no new dependency and no new
idiom:

    kill(pid, 0)

Signal 0 performs the permission and existence checks without
delivering anything. `Ok` means the process exists; `ESRCH` means it
does not. `EPERM` means it exists but belongs to another user — which
must read as ALIVE, not dead, since the question is existence.

Wrap it in one small helper with the unsafe block contained, matching
how the existing libc calls are wrapped. Do NOT shell out to `ps`,
`pgrep`, or `kill`.

## PID reuse, and why it is tolerable here

A recycled pid can make a dead task look live. This is bounded and
worth accepting:

- The hook's suppression is unaffected — it validates on the task id,
  never the pid — so a recycled pid can NEVER cause a missed wake. The
  worst case is a status line that shows attendance slightly too long,
  which is strictly better than today, where it shows attendance
  forever until the next Stop.
- `since` makes a stale line visibly suspicious on its own.

Do not add process-start-time comparison to close the reuse window
unless it can be done without a subprocess and without a new
dependency; the residual risk does not justify either.

## Where the pid comes from

This is the part the feature lives or dies on, and the first draft
skipped it. A background-task handle is a HARNESS handle — Claude
Code's task ids, opencode's `b…` ids — and the tool result carries no
pid, so an agent cannot derive one from the id it was given. If no
mechanism is documented, every marker stays pid-less and the liveness
check never receives an input.

The pid must come from the backgrounded command itself, which is the
only thing that knows it. The documented recipe is for that command to
record its own shell pid as its first act:

    echo $$ > .clank/agents/<label>/attending.pid
    <the actual long-running command>

and then, once the harness returns the task id, to pass both:

    clank attending <task-id> --pid "$(cat .clank/agents/<label>/attending.pid)"

`pgrep -f <pattern>` after the fact is the fallback when the command
was not launched that way. It is a fallback, not the recipe: it can
match the wrong process, and it fails outright for a command whose
pattern is ambiguous.

**Whatever is chosen, the Stop hook's attending hint and the clank
skill docs must teach it.** An optional flag nothing knows how to fill
is the same as no flag.

### CLI shape

`clank attending <task-id> --pid <pid>`, with `--pid` optional.

`AttendingArgs` (`crates/cli/src/cli/mod.rs:1673`) already has
`task_id: Option<String>` for `--clear`, so an added
`#[arg(long)] pid: Option<i32>` leaves every existing invocation
parsing exactly as it does now. The rejected alternative — one
positional accepting either form, sniffed by whether it parses as an
integer — makes the argument's MEANING depend on its content, which
breaks the moment a harness issues a numeric task id.

A marker without a pid must remain valid: it is still a correct
instruction to the hook, it merely cannot be display-verified. Render
it exactly as today, with no liveness claim.

## Required tests

- A marker whose pid is live renders as attending.
- A marker whose pid is gone renders as stale, and is NOT deleted —
  reaping remains the hook's authority alone.
- A marker with no pid renders as attending with no liveness claim,
  proving the old form still works.
- The liveness helper reports the current process as alive, and a pid
  it has just reaped (or one that cannot exist) as dead.
- `EPERM` reads as alive. Assert the mapping directly on the helper
  rather than trying to stage a foreign-owned process.
- The hook's suppression is unchanged by any of this: a live marker
  still silences every reason, keyed on the task id, whether or not a
  pid is present.
- No test spawns zellij, an agent binary, or the stop hook, and no test
  shells out to `ps`/`kill`.

## Out of scope

- Whether the tool's `background_tasks` can report an already-finished
  task as live. Observed once (three completed ids came back live) and
  still unexplained. It is the hook's problem, not status's, and this
  plan makes it MORE visible rather than papering over it: a pid that
  is gone while the hook still believes the task is live is exactly the
  evidence that bug needs.
- Making the HOOK check the pid too. Tempting, because it would make
  `attending` work on codex for the first time — codex omits
  `background_tasks` entirely, so today no codex marker can ever
  suppress anything and every codex marker is reaped at the next wake.
  But changing what the hook trusts is a change to suppression, and
  this plan is about making status honest. Worth its own plan once a
  pid is actually being recorded.
- Reaping stale markers from status. Status stays read-only; a reader
  that deleted markers would race the hook and could silence a wake the
  agent never acknowledged.
