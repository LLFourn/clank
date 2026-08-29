# attending-requires-a-knowable-end

> We can't let attending things happen unless we know exactly when the
> thing is finished, otherwise it defeats the purpose. — lloyd

`a-silenced-turn-needs-a-wake-channel` closed half of this while this
plan sat queued. What it did NOT close is the half lloyd's rule is
about, and the boundary is worth stating precisely so this plan does
not claim credit for work already done.

## Already fixed, do not re-fix

The reported symptom is gone. `record_attended` now runs only in the
suppress branch, and suppression requires proof (task listed live, a
live pid, a token still holding it). So an `attended` record can only
describe an attendance that proved itself, and this row:

    attending: claude → bk3qnvo12 · 2m

— a pid-less record, ageing upward forever because nothing could learn
the task had ended — can no longer be created. `clank attending` also
now SAYS when the marker it wrote cannot suppress.

## What is still broken

**Clank still ACCEPTS a marker it cannot verify.** It writes it, warns
about it, and then ignores it. The warning is honest but the record is
pointless, and the capability the agent wanted is simply gone:

- A harness background task hands back a task id and an output file,
  never a pid. So the commonest case cannot produce a usable marker
  AT ALL.
- Measured during the previous plan's implementation: every one of the
  background tasks run in that session produced an unusable marker,
  and every turn-end re-nudged about the live task. That is the loop
  attendance exists to stop, now reopened for the common case.

So the feature is currently honest and useless. The rule below is what
makes it honest and useful.

## The rule

> Attendance may be recorded only when clank can determine, at any
> later moment, whether the attended work has finished.

Two ways to satisfy it, and no third:

1. **A verified process identity.** `--pid` plus a `ProcToken` that
   still holds it. The token is what makes a bare pid trustworthy —
   pids are reused, so a recorded number can come back held by a
   stranger.
2. **`clank run --desc "…" -- <cmd>`.** Clank writes the marker and
   then becomes the command, so it knows the pid without being told
   and the harness task's lifetime is the work's lifetime.

Everything else is refused at write time with an error saying which
of the two to use.

## `clank run` is the enabler

Without it the rule removes the feature for its commonest case. A
harness background task hands back a task id and an output file,
never a pid; the documented `echo $$ > …attending.pid` recipe only
works when the agent authors the command that way, and a wrong pid
passed confidently is worse than none.

### A subcommand, not a flag (lloyd)

`--run` was the wrong shape. The two operations differ in the one
dimension this whole plan is about — LIFETIME:

- `clank attending <task-id>` records a marker about work happening
  elsewhere, and RETURNS immediately.
- `clank run -- <cmd>` executes a process and lives exactly as long as
  it does.

A command whose blocking behaviour flips on a flag is hard to reason
about anywhere; in the one subsystem whose entire subject is when
things start and stop, it is self-defeating.

The flag matrix says the same thing more bluntly. Under `--run`,
`task_id` is meaningless (there is no id yet), `--pid` is meaningless
(clank supplies it), and `--clear` is meaningless (nothing to clear).
Three of the four existing arguments would need `conflicts_with`, on
top of the `required_unless_present` already there for `--desc`. That
pile-up is the type system reporting that these are two commands.

So: `clank run`. The name is free, one word like every sibling verb,
and it says what it does — clank supervises the process, which is the
whole reason to launch it this way.

`clank attending` keeps its current job unchanged and gains the
refusal rule above.

### The correlation problem (codex on d172ced)

`clank run` cannot satisfy the CURRENT suppression proof, and the first
draft of this plan promised a test that is impossible:

- Claude assigns `background_tasks[].id` only after launching the
  background command. `clank run -- <cmd>` runs INSIDE
  that command, so at marker-write time it knows its own pid and
  token but NOT the enclosing harness id.
- So `marker.task ∈ live_ids` — the first conjunct of `provably_live`
  — can never hold for a `clank run` marker.
- Run in the foreground instead and attendance is pointless: the turn
  cannot end until the command does.

### The handshake: the description IS the correlator

One protocol, chosen (lloyd: no prepare step). Everything below uses it.

The hook correlates by reading `background_tasks[].command` — the line
the agent backgrounded. So whatever identifies the run must already be
IN that line at launch. Clank cannot mint anything at startup: by then
the harness has captured the string, and a generated id would live
only in the marker.

`--desc` is already required, already on the command line, and already
in the marker. It is therefore the correlator, and no new ceremony is
introduced:

    clank run --desc "test run" -- cargo test

A prepare/launch protocol handing out a unique nonce was the
alternative and is REJECTED: it is two steps an agent can skip, and
the failure of skipping step one is silent.

### Correlation is structured recognition, not substring matching

The cost of reusing the description is that descriptions are not
unique. That is handled by being strict, not by hoping (codex on
adec1e1):

- **Parse, do not `contains`.** Recognise the command as a `clank run`
  invocation and extract its `--desc` VALUE, the way `is_clank_wait`
  recognises a backgrounded wait rather than grepping for a word.
  A description appearing anywhere else in an unrelated command line
  is not a match; substring matching is not identity.
- **Exactly one live match, or no suppression.** Zero matches means
  nothing proves the work is running. TWO OR MORE means clank cannot
  tell which is this marker's, and a coin-flip between two live tasks
  is exactly the guess this whole plan exists to refuse. Both cases
  park.

So two concurrent runs sharing a description degrade to nudges, not to
a wrong silence — the same failure direction as every other unprovable
case here.

This replaces `provably_live`'s first conjunct: "task ∈ live_ids"
becomes "exactly one live background task is recognisably this
marker's run". The id path stays for markers a caller supplies by
hand with `--pid`, so this is an addition, not a replacement.
**That makes the correlation half of the suppression rule this plan's
business**, so it is in scope here rather than deferred.

### Wrapper lifecycle: write the marker, then `exec`

Specified before coding, because a spawn-and-wait wrapper has a
failure mode that would undo the plan: killed while its child
continues, the harness task ends while the attended work survives —
liveness lies again, in the opposite direction.

So on Unix the wrapper does not spawn and wait. It writes the marker
and then `exec`-replaces itself with the command. One process
throughout, which keeps every property aligned at once:

- **Process identity.** The pid and token in the marker stay valid
  after the exec, because the pid does not change — so the marker
  identifies the real work rather than a supervisor of it.
- **Harness lifetime.** The harness's background task ends exactly
  when the work ends. There is no wrapper to outlive or predecease it.
- **stdio.** Inherited, untouched. No pumping, no buffering, no
  interleaving bugs.
- **Exit status and signals.** The command's own, natively. No
  propagation logic to get wrong, and no wrapper to swallow a signal.

The command line the harness recorded is unaffected — it captured the
string at launch, before the exec — so the `--desc` correlation still
works afterwards.

Failure to exec (bad command, not found) must exit non-zero AND leave
no marker behind, since nothing is running to attend.

### Correcting what owning the process actually buys

The earlier draft claimed this supplies "a wakeable end" —
`waitpid` on an owned child rather than sampling a pid. That is wrong
and codex caught it: clank is not running when the child exits, and a
`waitpid` inside the wrapper cannot wake the agent. **The wake channel
is still the harness's task-completion notification, exactly as it is
today.**

What owning the process genuinely buys is that the wrapper IS the harness's
background task and stays alive until the child exits. So the harness
task's lifetime tracks the real work instead of merely being adjacent
to it, and the liveness signal becomes honest — which is what
suppression needs. It makes an existing channel trustworthy; it does
not add one.

## Legacy records need almost nothing

Both files self-clean: `consume_attending` removes the marker AND the
`attended` record at every hook entry, unconditionally. So a pid-less
record written before the previous plan survives only until that
agent's next turn-end, in any repo. There is no migration to write.

What remains is narrower: the READER stays lenient — old shapes must
keep parsing rather than erroring, exactly as `desc` did — and a
record without a pid must never render as a live wait during the
window before it is swept. Verify the current renderer against that
rather than assuming; the previous plan changed which records can be
CREATED, not how an existing one is drawn.

## Tests

Admission:

- `clank attending <task>` without a verified pid is REFUSED, and the
  error names both remedies: `--pid`, or `clank run`.
- `--pid` without a token, or a token that no longer holds the pid, is
  refused the same way.

Correlation — the launch-order test (codex on d172ced):

- **Model the real sequence.** The harness launches
  `clank run --desc "test run" -- cmd` and only THEN assigns a task
  id; the marker never sees that id. Prove suppression still happens,
  by recognising the description in `background_tasks[].command`. A
  test that hands the marker a task id it could not have known would
  pass while the feature cannot work.
- **Recognition is structured.** A command that merely CONTAINS the
  description — `echo "test run"`, or a `clank run` whose `--desc` is
  something else with the text elsewhere on the line — is NOT a match.
- **Exactly one match.** Zero live matches does not suppress. Two live
  `clank run` tasks sharing a description do not suppress either, and
  that ambiguity is asserted directly rather than left to chance.
- The `--pid` id path still correlates, so description matching is an
  addition and not a replacement.

Lifecycle:

- The marker is written BEFORE the exec, so it exists for the whole
  life of the work rather than from some point after it started.
- A command that cannot be exec'd exits non-zero and leaves NO marker
  behind — nothing is running, so nothing may claim to be attended.
- The recorded pid is the pid the work runs under, asserted after the
  replacement rather than assumed from before it.

Carry-over:

- A legacy pid-less record parses, and renders with no liveness claim
  during the window before the next hook entry sweeps it.
- A marker written by `clank run` satisfies the suppression proof from
  [[a-silenced-turn-needs-a-wake-channel]] with the caller passing
  nothing but `--desc`.

## Out of scope

- The suppression rule's PROOF requirement (pid + token), which
  `a-silenced-turn-needs-a-wake-channel` settled. Its correlation
  half changes here, for the reason given above.
- Reaping. Records are still the hook's to consume; this plan changes
  what may be WRITTEN and what may be SHOWN.
