# the-turn-end-is-one-decision

The stop hook throws away its types at every boundary — the wake it
reads and the decision it makes — and the agent falls through the
gaps that leaves.

> "our stop hook sounds wildly too complicated here. Can you rewrite
> the drafted plan to consider a full rearchitecture of it. It sounds
> like it is the accumulated drift of various complaints I've made
> about it." — lloyd
>
> "why wouldn't this be an enum. Looks like lots of strings here
> should be enums? … right so can you stop doing stringly typed
> stuff." — lloyd

## The drift, measured

`cli/stop_hook.rs` is **4,956 lines and 181 functions**;
`core/hook_io.rs` adds 875. Between them their comments cite **21
distinct plans** as the reason some branch exists:

    a-session-binds-itself-at-start       attending-suppresses-standing-wakes
    adhoc-continue-wake-investigation     attending-checks-in-when-the-work-runs-long
    auto-mode-default-on                  claude-asyncrewake-work-loop
    codex-idle-is-not-a-hook-failure      external-wake-hints-carry-payload
    github-watch-prompts                  grok-first-class
    nothing-refuses-in-silence            remove-wait-timeout
    soft-disallow-multiple-plans          terse-arm-wait-nudge
    the-nudge-names-the-command           wait-output-is-a-minimal-hint
    wakes-state-their-discharge-condition …and five more

Every one was a real complaint, and every fix landed where the
complaint was noticed rather than where the decision lives. This is
not a design with bugs in it. It is twenty-one decisions with no
design.

## One missing model, at two boundaries

The hook reads its own binary's output and throws the types away, then
makes its decision in untyped control flow. Same disease, two places —
which is why this is one plan and not two.

**Boundary 1 — the wake it reads.** `clank wait --json` is produced by
`WaitItem`, a Rust enum tagged on `kind`; its `reason` is
`WaitingReason`, also an enum, also with an `as_str()`. The hook
spawns **itself** (`std::env::current_exe()`, `stop_hook.rs:783`),
reads that JSON back, and lands it in `kind: Option<String>`,
`reason: Option<String>`. One variant list is then maintained by hand
in four places: `discharge_of` (~25 `&str` arms), `render_wait_items`
(22), `wire_tag_of` (21), and a test's four hand-typed lists.

`wire_tag_of` exists because someone hit this — its doc says adding a
variant "without classifying it fails to compile … which is how
`fix_commit_tag` reached the catchall unnoticed." **It was never
called.** Dead, guarding nothing, while `command_event`'s
classification went stale underneath it: it told agents "change or
remove the command source to stop it" (true when an exiting producer
WAS the event, advice to break your own watch once it became a WAL
row), and its renderer read `exit_code`/`output_tail`, fields the wake
had not carried for three commits.

And the test named `every_wait_item_variant_is_classified` does not
iterate over every wait item variant. It iterates over lists someone
typed, asserting that strings in one list match strings in another.
It cannot fail for the thing its name claims.

**Boundary 2 — the decision it makes.** Ten `SilentReason` variants,
each its own `return` in a linear function, evaluated in an order that
matters and is written down nowhere. Seven control files in one agent
directory (`attending`, `attended`, `attending.holder`, `turn.end`,
`wait.gen`, `wait.holder`, `wait.lock`). Six overlapping answers to
one question — "is something already watching this agent?": the tool's
`background_tasks` list, the attending marker triad, the `wait.lock`
flock, `wait.gen`, `wait.holder` plus a 60×1s stale-handoff retry, a
`generation_changed` poller on a 2s tick, and the `--die-with-owner`
stdin pipe.

And **two different standards for the same word**: `provably_live`
requires three signals — listed, live pid, token still held.
`has_background_clank_wait()`, in the same `impl` seven lines away,
requires one: that it appears in a list whose own doc says it names
tasks that already finished.

## The invariant, and why the structure makes it easy to break

**A turn clank is RESPONSIBLE FOR CONTINUING ends either armed, or
yielding to a named waker that is live. There is no third state.**

The scope clause is the correction. An earlier draft said "every
turn", which is false and would have broken a deliberate behaviour:
`AutoOff` arms nothing ON PURPOSE, and outranks even a roster error.
A passive adapter has no continuation to own. A superseded session
must not act. None of those can honestly manufacture a waker, and
demanding one would have turned an intentional stop into a bug
(codex 932cc94).

So the decision names both halves, and which half it is in is the
first thing it says:

    TurnDecision
    ├─ NotOurs(why)     clank is not continuing this turn
    │                   AutoOff · Superseded · Unsupported · Unresolved
    ├─ Failed(what)     we meant to decide and could not
    ├─ Yield(waker)     someone else wakes it, and here is WHO
    └─ Arm(intent)      we park a waiter

The invariant is a property of the `Yield`/`Arm` half only, and it is
exhaustive over the enum: a `Yield` cannot be constructed without a
waker, so the third state stops being representable. `NotOurs` is
honest — nobody will wake this agent, and that is the configured
intent — and it is exactly the distinction the current code cannot
draw, because a `SilentReason` label reads the same either way.

**`Arm` is an INTENT, not an outcome.** A pure decision cannot
establish that the lease was taken or the child spawned. The effect
layer reports back, and an arm that failed is a `Failed`, never a
silent success — which is today's behaviour at `asyncrewake_park`'s
lease error and must survive the move.

The third state is where agents die, and nothing in the code can even
express the assertion. The gates are `return` statements; "did any of
them leave nobody watching?" is not a question the shape can be asked.

Worse, the one load-bearing effect sits *inside* the gauntlet.
`record_turn_end` is at `stop_hook.rs:227`, below the `YieldArmed`
return at 195 — so the most common suppression path **writes nothing
at all**, not even that a turn happened. An agent that has yielded
forty times is indistinguishable, on disk, from one that has not taken
a turn since yesterday. That is not a missing log line; it is the
diagnostic consequence of putting an effect in the middle of a
decision.

## The bugs are instances, not a list

**A finished task counts as a live waiter.**
`has_background_clank_wait()` (`hook_io.rs:306`) has no status or
liveness filter, so a completed `clank wait` yields the turn — forever,
since nothing re-checks and the yield leaves no trace. Observed live in
`~/src/safepay`: `wait.gen` 8, `wait.holder` 7, `turn.end` still
stamped generation 7 from the previous day, no waiter process, session
alive and answering pings. It is the three-signal rule the repo already
wrote, not applied here.

**Zero waiters is undetectable.** All six mechanisms above manage *too
many* waiters. Not one notices *none*. The evidence is on disk
(`wait.gen` ahead of `wait.holder`, no process) and nothing reads it
that way.

**A lock that can be held by someone obsolete is not a lock.**
`wait.lock` guards *the park* rather than being held by *the current
incarnation*, so a dead generation can hold it. That is the entire
reason `wait.gen`, `wait.holder`, `park_decision` and the 60-try
handoff exist — three mechanisms adjudicating a fourth that asked the
wrong question.

**Orphaned hooks.** A `stop-hook --loop asyncrewake` with no wait
child, running over an hour, was found during this diagnosis — the
argument that `--die-with-owner` is not sufficient today.

**A stale discharge and a stale renderer**, above: the same missing
type, surfacing as wrong text instead of a wrong wake.

## The build

### Milestone 1 — the wake is typed

`WaitKind` in **core**, beside `WaitItem`, with `WaitItem::kind()` an
exhaustive match returning it. One definition at the definition site:
a new variant fails to compile in `core`, before the hook is reached.
`wire_tag_of` deletes — it was the hand-rolled stand-in for exactly
this.

`discharge_of(WaitKind, Option<WaitingReason>) -> Discharge`: **total**,
no `_ => return None`. `master` keys on `WaitingReason` exhaustively.
`render_wait_items` matches the same way.

**Exactly two `None`s survive**, both at the parse boundary: unknown
`kind`, unknown `reason`. That is the real constraint and the only one
— the binary on disk can be replaced by `cargo install` while an agent
is parked, so deserialization must tolerate a tag this build has no arm
for. `Option<WaitKind>`'s `None` IS the fail-soft. It justifies a
fallible `String → WaitKind` at the edge and nothing past it.

First, because milestone 2's evidence type carries these. A decision
function over stringly-typed evidence is stringly typed.

### Milestone 2 — the decision is a value

    fn decide(evidence: &TurnEvidence) -> TurnDecision

`TurnEvidence` is everything the gates read today — tool, typed hook
input, config, attendance record, waiter state — gathered ONCE up
front. `TurnDecision` is the enum above, and a `Yield` cannot be
constructed without a waker, so "silently nobody" stops being
representable.

**Each waker kind carries the evidence it actually has.** One
universal "listed + live pid + matching token" rule is not
implementable: that is the ATTENDING record's evidence, because the
marker stores a pid and a token. `HookInput::BackgroundTask` carries
only `id`, `status` and `command` — no process identity at all — and
a hook-owned wait or a check-in timer need not appear in that list
(codex 932cc94). So the type is per-path and the predicate is total
over it:

    Waker                        evidence it has
    ├─ Attended  { task, pid, token }   all three — today's `provably_live`
    ├─ Harness   { id, status }         a list, and no way to correlate it
    ├─ OwnedWait { child }              this hook spawned it; it has the pid
    └─ CheckIn   { deadline }           this hook holds the timer itself

    fn liveness(&self) -> Liveness      Live · Dead · Unknown

`Harness` did not survive contact: a list cannot be correlated, which
is what the next section is about, and the row was replaced by the
registration the tie is constructed from. What milestone 2 builds is
those two:

    RegisteredWait { tag }        the token in the claim and in the
                                  listed command — one name, two places
    AttendedTask   { task, pid }  the marker's correlation and the pid
                                  its stored ProcToken still holds

**A waker value is constructible only where its proof is.** Deriving
one from a finished turn's `SilentReason` reads back a label, and a
label is not evidence: the same `YieldArmed` was written by the path
that proved a wait live AND by codex yielding to a bare `sleep 60`,
so the derived decision announced a registered wait that did not
exist (codex d281959260a2). The rule the type enforces is that the
decision is BUILT at the site holding the proof and the wire label is
derived FROM it — never the reverse.

The same collapse was found twice more underneath, each a pair of
answers sharing one name: `BgDisposition::YieldArmed` meant both
"a registered wait is live" and "this tool has no auto-wake", and the
check-in's `armed` bool meant work ended, a newer attendance took the
timer, or `--clear` cancelled it. Both are now variants, which is what
made `AttendanceCleared` a turn-end clank can report rather than a
silence indistinguishable from a live waker.

**`Unknown` is not a licence to sleep.** That is the whole bug in one
word: the harness list's own doc says it names tasks that already
finished, so `status: completed` is `Dead`. A `Yield` requires `Live`.

**And the observation turned out not to be ACQUIRABLE.** Three
attempts each looked like proof and proved something else (codex
861208f, 2f38deb, 07c7d8d):

- the LIST alone — its own doc says it names finished tasks;
- a live pid — the OS reuses numbers, so it can be a stranger;
- an inherited session variable — every descendant of a tool shell
  has it, including a detached wait with its output redirected to a
  file, whose completion notifies nobody.

The reason is structural. A `BackgroundTask` carries `id`, `status`
and `command`: no pid, no process identity. **Nothing the harness
reports ties a listed task to a running process** on its own.

**So the tie is CONSTRUCTED.** The arm instruction carries a fresh
token — `clank wait --tag <token>` — the agent runs that command, and
the wait publishes the same token in its claim. One token in two
places names one process: the listed task and the claimed pid. That
is a registration rather than an inference, and it is what `Live`
requires.

Deferring it was not an option, and finding that out cost a round.
With `Harness` permanently `Unknown`, the legacy adapter re-issued
its arm instruction on every stop — including to an agent that had
already obeyed — so three turns produced three live waiters, all
healthy and all ignored (codex bedc480). "A repeating nudge is
recoverable" described a hot loop that burns turns and accumulates
processes. An instruction the agent cannot satisfy is not a
continuation.

**Clank still owns the other end, and the claim still earns its
place.** A `clank wait` publishes what it is — birth identity, the
session it was launched inside, the incarnation that claimed the
label — because milestone 3 fences on that record, milestone 4 reads
it to find an agent with no waker, and a second wait arming for one
agent says so today instead of silently doubling.

**Three fields, because one would be weaker than what already
exists.** `Attending::provably_live` checks exact task correlation
AND a `ProcToken` process-birth identity, and an earlier draft here
asked only for a live pid and a current token — which a killed waiter
defeats the moment its pid is reused, and which a separately launched
wait for the same label satisfies without being the thing that will
wake this session (codex f3edd17):

    birth        a ProcToken, not a pid — pids are reused
    incarnation  the fencing token; whose claim this is
    delivery     the session this process was LAUNCHED INSIDE

`Live` would require all three AND a correlation to the listed task.
The first three the claim provides; the fourth the harness does not,
which is why that verdict is currently unreachable.

The claim record is the same one milestone 3 fences on. One artifact,
two readers — which is the point, since a second record of "who is
watching" is what this file already has six of.

**When the claim is absent** — a wait from a binary older than the
record, or a harness task that is not ours — the answer is `Unknown`,
and `Unknown` does not suppress. That is a behaviour CHANGE and the
sequencing note says so: milestone 2 is otherwise a pure refactor,
and this one rule is the exception, landing with the test that pins
it.

**What each adapter DOES with `Unknown` is per-adapter, because their
continuations differ.** An in-hook adapter (codex, opencode) parks.
Legacy claude's `BackgroundArm` path deliberately never parks in its
synchronous hook. With no immediate work, it nudges the harness to
launch a background wait, whose completion delivers the work
(`claude-stop-hook-minimal-hint`). So
`Unknown` there means the nudge, NOT a park: starting a detached wait
from that hook registers no completion notification, and parking
changes that adapter's contract (codex f3edd17). Calling a decision
`Arm` does not establish how its output reaches the agent.

And the nudge TERMINATES, because it carries the registration: the
agent runs the tagged command, the wait publishes the token, and the
next stop recognises it and says nothing. Without that the legacy
path re-asked forever and the agent accumulated waiters, which is not
a continuation however recoverable it sounds (codex bedc480).

A wait launched some other way — by hand, or by a binary older than
the token — carries no registration and is `Unknown`, so the hook
asks for a tagged one. That costs a duplicate waiter once, and the
duplicate says so; it does not cost an unbounded sequence of them.

Pure, so the gauntlet becomes table-testable with no process, repo or
clock. Most of those 181 functions exist to make the impure version
testable at all.

**Effects move OUT and run after.** `record_turn_end` first,
unconditionally, because the turn ended regardless of what is decided
about it.

**The silent set is closed.** Every `SilentReason` is now either an
intentional stop or a yield built from live waker evidence at its
producing site. `NoWork` left after auditing normal `clank wait`'s
exit paths: only `--peek` intentionally returns empty success.
`PeekFailed` became a diagnostic carrying the error. OpenCode's
`PollDeadline` became a plugin renewal action, guarded by idleness
and session activity. `BusyOwnWork` was the last unclassified path:
a listed Claude task has no process identity, so its completion is
not a proven wake. When the peek finds ready work, the hook now
renders those items as a continuation. It tells the agent to await
its background task within that turn if the task blocks the work;
asking it to start a wait for already-ready work was the hot loop.
With no ready work, the tagged wait nudge still parks alongside the
task.

**Supersession now reaches the wait's own loop.** A normal `clank
wait` captures the label's generation. Its existing filesystem wake
or heartbeat checks that token before projecting work and before
returning items. A changed generation exits the child with a typed
control result; the hook reads it as `StaleGeneration` and delivers no
work. The hook holds no park lease and runs no separate supersession
poll. The attended check-in timer uses its existing liveness tick to
notice the same change. A successor's wait child takes the claim only
after the predecessor withdraws or its exact process identity is gone;
a live predecessor beyond the handoff bound produces a failure.

## Revision: the hook runner is not the pane

Swapping a running Codex into its roster pane left the label unbound,
and every Stop then cleared a bind that did land. Same fact, two
misses.

The Stop hook and SessionStart do not run in the agent's pane. Codex
runs them in a long-lived app-server daemon that keeps the
`ZELLIJ_PANE_ID` and `CLANK_AGENT` of whichever pane started the
daemon. That pane can be gone while `codex resume <session>` is alive
in another. The guard treated the daemon's missing id as proof the
agent was gone, and released the label at every Stop — a bind lasted
one turn. Turning the guard off for every non-Claude tool stops the
false release and also stops noticing a pane that really closed.

SessionStart only binds when that same daemon environment carries
`CLANK_AGENT`. A zellij swap of an already-running process fires no
SessionStart at all, and a resume whose hook sees no label takes the
other branch, which mints a generation and does not bind. Nothing else
looked at the process now sitting in the pane, so the label stayed
empty until someone typed `clank as`.

The pane that is running the session is the evidence. A process whose
command is `resume <session>` and whose `ZELLIJ_PANE_ID` is still
listed has not lost its pane, and if that pane is this repo's
`clank agent start <label>`, that label is bound to that session —
including when no hook ran. The daemon's own id never answers either
question. A failed listing or a failed `ps` is not a death.
Nor is a successful `ps` with no matching `resume <session>` line:
the agent might have started without that command line, or the process
listing might omit its environment. Releasing a binding needs a pane
previously and positively associated with THIS session, followed by
proof that pane is gone. Until that association exists, the answer is
unknown and the binding stays.

**"Gathered ONCE up front" is a behaviour change, not a refactor,
and the order it disturbs is load-bearing.** The yields short-circuit
BEFORE the ROLE is resolved, so a turn with a live registered wait
never reads the roster: an agent in no roster still yields to the
wait that is demonstrably watching it. Hoisting that read to build
`TurnEvidence` turns the same turn into a `Failed`, and an agent
being woken correctly starts reporting a diagnostic instead.
`a_live_registered_wait_outranks_a_role_that_will_not_resolve` is
the tripwire; the priority was positional and therefore untested.

The AGENT CONFIG is the same hazard, and whether it is reachable
depends on WHICH identity path ran — a distinction worth stating,
because getting it wrong drops a supported branch from the migration.

`resolve_identity_for_hook_in` has two:

    explicit label   `CLANK_AGENT` is returned BEFORE any config is
                     read. Identity succeeds whatever config.json
                     holds, so the strict load below the yield is the
                     first read to fail — and the yield above it wins.
    session lookup   the fallback reads every config lossily to find
                     the binding, so a config that will not parse
                     fails IDENTITY one step earlier, above the stamp
                     and above the yields.

So the config-versus-yield case IS reachable, just only under an
explicit label (codex efe2f6286cf0, reproduced through the CLI). Both
are pinned:
`a_live_wait_outranks_an_unreadable_config_under_a_stated_label` and
`a_live_registered_wait_outranks_a_role_that_will_not_resolve`.

Which is the point worth keeping: the fix is not "make gathering
lazy" but "know which reads are actually below a short-circuit, on
every path that reaches them". Errors can travel IN the evidence as
`Result` fields and be inspected in the current order (codex
31b4b49f), so eager gathering is available — it just may not silently
promote an error past the branch that used to win.

### Milestone 3 — one fencing token, checked in one place

The earlier draft said to move the flock from "the park" to "the
incarnation" and retire the generation. That was hand-waving, and the
reason is worth stating: **a lock does not revoke a live holder.** One
lock shared between incarnations lets an obsolete owner block its
successor forever; one lock each lets both act. Neither is fencing
(codex 932cc94).

The generation counter is not incidental, then — it IS the fence, and
what is wrong is that its job is spread across four mechanisms. So
this milestone keeps the guarantee and collapses the machinery:

- **What a fence is here.** A monotonic token minted by whoever claims
  the label. It does not stop an obsolete holder from RUNNING; it
  stops its output from counting.
- **One `am I still current?`**, asked at one place, replacing the
  `generation_changed` poller, `park_decision` and the 60×1s handoff
  retry.
- **One rejection point.** Every effect a hook publishes — the wake, a
  `turn.end` stamp, an attendance record — passes it. Today that is
  the wake guard alone, and everything else is ordering.
- **The WAIT owns the claim, not the hook.** Named, because leaving it
  to "a file exists" is what produced six mechanisms (codex 861208f).
  A hook is short-lived and holds nothing durable; the wait child is
  the long-lived process, it already publishes its pid and token for
  the liveness join in milestone 2, and it is the thing that must
  actually die. The hook's park ending is a consequence, not the
  mechanism.
- **A reused session id is not a new incarnation.** `--resume` keeps
  the id, so the fence cannot key on it. It keys on the CLAIM.
- **An idle predecessor still has to LEARN it is obsolete**, and a
  publication-time fence cannot tell it: a parked wait may emit
  nothing for hours, so nothing of its ever reaches the check. An
  asynchronous observer is therefore RETAINED rather than retired —
  but it moves into the wait's existing select, beside its sources,
  where noticing a superseded claim is one more branch rather than a
  standalone 2s poller. The wait exits; `--die-with-owner` and the
  drop guard take its process group with it.
- **Revocation is a REQUEST; release is an ACKNOWLEDGEMENT, and they
  are different states.** An earlier draft made "the record replaced"
  a completion condition, which proves only that the SUCCESSOR wrote
  something. A suspended predecessor satisfies that instantly while
  its waiter is still live — two waiters, the exact thing this
  mechanism exists to prevent (codex f3edd17). So the claim carries
  the requested incarnation AND the acknowledged holder. One
  artifact, two fields, never one: that distinction is what `wait.gen`
  and `wait.holder` encode today, and collapsing them was erasing a
  real state rather than removing a duplicate.
- **The predecessor's identity survives the update**, so "gone" is
  checkable about the right process rather than about whoever holds
  that pid now.
- **Takeover completes on release or verified exit.** The successor
  owns the label when the predecessor acknowledges — or when its
  process-birth identity is verified gone, which covers a predecessor
  that dies before it can answer. A predecessor that neither
  acknowledges nor dies within the bound is a `Failed` naming it:
  never an unbounded spin, and never a second waiter admitted by
  default.

Whether `wait.gen`, `wait.holder` and `wait.lock` survive as files is
an implementation question. The revocation guarantee they provide is
not negotiable, and this milestone is judged on preserving it.

What collapses is the count, not the capability: a claim record the
wait owns, observed in a loop it already runs, checked at one
publication point — in place of a lock that cannot revoke, a counter
read from three places, a poller of its own, and a 60-try retry that
exists because the poller is asynchronous.

The hook must pass its input session and captured generation to the
wait child. Codex's long-lived app-server can carry an environment from
the pane that launched it, even when a different pane owns this turn;
the child's ambient session then describes the daemon, not its wake
destination. Capturing the generation once also prevents an old hook
from adopting a successor's incarnation after a delayed park. The
claim and handoff then move into the wait child: under the claim lock,
it keeps a live predecessor's full process-birth identity intact until
that predecessor withdraws or is verified gone. A second wait for the
same generation and session returns an explicit already-parked control
response. The hook no longer takes `wait.lock`, writes `wait.holder`,
or polls for a lease handoff. It still reads legacy holders for health
checks while processes from an older binary can be running.

### Milestone 4 — the unreachable agent is reported

`clank doctor` and the TUI name an agent whose LAST TURN ENDED and
which has no live waker.

Both halves matter. A stale `turn.end` stamp is not idleness: an agent
mid-turn legitimately has no parked waiter, and reporting that would
make the check noise on every healthy busy agent (codex 932cc94). The
report is about a turn that has ENDED — the stamp `record_turn_end`
now always writes — with no `Live` waker and no `NotOurs` decision
explaining the quiet.

This is the assertion the whole plan exists to make checkable, and it
belongs where a human sees it.

## Tests

- **The invariant, as a property**: over a table of evidence, every
  decision is `NotOurs`, `Failed`, `Arm`, or a `Yield` whose waker is
  `Live`. Exhaustive over the enum, so a new variant cannot be added
  without landing in one of those.
- **`AutoOff` still arms nothing**, and still outranks a roster error.
  The scope clause exists for this case; a test that only proved the
  new refusals would let it be "fixed" into a bug.
- A `Waker` whose liveness is `Unknown` does NOT suppress. Per kind,
  since the evidence differs: `Harness` has no pid to check, and that
  is the case that currently lies.
- A `clank wait` the harness calls `completed` is `Dead`; any other
  listed one is `Unknown`. Pure input, no process.
- OBEYING the arm instruction ends it: nudge, run the tagged command,
  and the NEXT stop is quiet. Asserting only "not YieldArmed" passes
  for the hot loop this is meant to rule out, so the test walks the
  turns.
- A listed `clank wait` does NOT yield on a claim that passes every
  other check — this live process, this session, the current
  incarnation — when its token names no listed task. None of those
  shows the listed task IS the claimed process, and asserting the
  refusal with the claim in place is what stops the yield creeping
  back on a future guess.
- Two live listed tasks carrying one token is `Unknown`: "which
  process is this?" must not become a guess again.
- Every adapter still has a DELIVERABLE continuation for `Unknown` —
  including the legacy one, exercised with asyncRewake unavailable,
  where the continuation is its nudge and not a park.
- Every existing `SilentReason` has a case that still reaches its
  decision. Twenty-one plans' behaviour is being moved, and what is
  still right must be pinned BEFORE the move, not rediscovered by an
  agent going quiet in production.
- Each discharge's TEXT: an event owed until an ack names
  `clank events ack`; a blocked wake says the agent cannot clear it.
  These are what would have caught the stale `command_event` text,
  which passed `Until(_)` for three commits.
- An unknown `kind` and an unknown `reason` still parse and render
  loose — pinned on the REAL wire, a value serialized from a
  `WaitItem`, never a hand-written JSON literal. A hand-written
  fixture is how `exit_code`/`output_tail` outlived the fields it
  named.
- A yielded turn stamps `turn.end` — asserted on the file, since the
  file is the only evidence a human gets.
- **Fencing, stated as behaviour rather than as files**: a
  same-session `--resume` with the old hook still alive (the id is
  reused, so the fence cannot key on it); a concurrent handoff, where
  exactly one incarnation's output counts; cancellation of the
  predecessor's wait child, which is what makes a takeover finish
  rather than spin; and an ALIVE predecessor whose wait produces no
  events at all, which a publication-time fence cannot see and the
  observer exists for.
- **A visible new claim is not a completed handoff.** The predecessor
  is alive and has NOT processed revocation: the successor does not
  own the label yet, and no second waiter runs. With a timeout, and
  with two successors racing — exactly one wins, and the loser is a
  `Failed`, not a silent co-owner.
- A predecessor that neither acknowledges nor dies within the bound
  is a `Failed` naming it — not a spin, and not a second waiter.
- **PID REUSE does not prove liveness**: a claim whose pid now belongs
  to another process is `Unknown`. And an unrelated live waiter for
  the same label, beside a stale listed task, is `Unknown` too —
  correlation, not coincidence.
- A stale incarnation's WAKE is rejected — the guarantee today's
  generation check provides, asserted on the output rather than on
  the counter, so the test survives the mechanism changing.
- An agent whose LAST TURN ENDED with no live waker is reported by
  name; an agent MID-TURN with no parked waiter is not. The second
  half is what keeps the check from firing on every healthy agent.
- **No test asserts that every variant is classified.** The compiler
  does that, and a test claiming a compile-time property at runtime is
  what this plan removes.

## Out of scope

- The wake PAYLOAD — what a hint says and how it is phrased
  (`wait-output-is-a-minimal-hint`, `the-nudge-names-the-command`,
  `github-watch-prompts`). Rendering, and it is fine.
- The rest of the hook's projection: `plan`, `sha`, `pr`, `round` and
  the other payload fields stay `Option<T>`. They are data, not
  discriminants, and nothing branches on them.
- `WaitItem`'s wire format. Tag strings do not change, so a parked
  agent's in-flight JSON still reads.
- The tool's own liveness reporting. Clank cannot make
  `background_tasks` carry a pid, and does not try to; it corroborates
  that list against a record it owns, and stops treating a list that
  admits it names finished tasks as proof of a live one.
- `clank wait`'s SOURCES, WAL and event handling — what it watches and
  how it logs. Its INCARNATION OWNERSHIP is in scope, because that is
  where the long-lived process is: it publishes the claim milestone 2
  joins against and observes the supersession milestone 3 fences on.

## A note on sequencing

Milestone 1 is a refactor with no behaviour change. Milestone 2 is
one too, with a single deliberate exception — `Unknown` stops
suppressing — which is the safepay bug and lands with the test that
pins it. Their value is that 3 and 4 become small. Doing it the other
way round means adding a seventh mechanism to the six that exist,
which is how this file reached 4,956 lines.

## Verification note: status watcher under the tool sandbox

The status watcher test's tracked-edit assertion fails on this head
and on an isolated archive of `932cc942^`, before this plan's code.
The watcher implementation and test assertion are unchanged between
those revisions. In this environment notify's native watcher attaches
but does not deliver the tracked edit. The test now runs the same
`WakeFilter` callback through notify's polling backend, which supplies
events here; production continues to use the native backend. Paths
from the polling backend can use `/var` while the filter's repo root
is canonicalized to `/private/var`, so the filter canonicalizes such
event paths before its ignore check.
