# nudges-are-on
# An agent that stops with work owed is nudged, and one whose turn failed is retried

## Why

`the-agent-state-drives-nudges` built the nudge, but every automatic send is gated on the
capability table in `resume.rs`, and that table shipped empty. So every harness is
status-only. An agent that stops with work owed, or whose turn dies on "Selected model is
at capacity", is reported as stuck and left there. That is what happened to codex for 7
hours, and the reason the plan existed was to nudge it. This plan switches the nudge on.

## What to build

One implementation commit, through the existing `Driver`, claims, budgets and history. No
new recovery owner.

### 1. Observe, and commit the result either way

The master runs one throwaway Claude Code session and one throwaway Codex session. Each
runs in a scratch directory outside any repo, launched the way clank launches agents. In
each:
- start a long turn;
- while it runs, type a line and submit it exactly as `pane_input::submit_async` does;
- record whether the running turn completes and the typed line runs after it (queued),
  or the turn is aborted.

Each result is committed as an `Evidence` row:
- the harness and the exact version observed;
- the transport (typed submit);
- the outcome, `Queues` or `Aborts`;
- the steps and what was seen;
- the documented draft race: a person's half-typed input after the final guard is
  submitted with the hint.

Both outcomes complete the plan:
- **A `Queues` row** enables that harness's typed submit.
- **An `Aborts` row** is a committed negative result. That harness's typed path stays
  disabled, and its state says why (`typed input aborts a running turn`). The plan
  finishes with the limitation reported to the user, never disguised as nudging.

### 2. Version policy

A row states the version it observed. Enabling later versions is a policy, not further
evidence, and is recorded as such:
- `Evidence.from` is the observed version.
- `applies_to_later: true` is the forward-compatibility assumption, named as a policy in
  the row and in the agent's reason (`allowed by policy beyond observed 2.1.280`).

Resolution is deterministic, with revocation taking precedence:
- for an installed version `v`, take the rows for its harness and transport whose `from`
  is at or below `v`;
- the one with the greatest `from` decides;
- an `Aborts` row with a `from` at or below `v` and greater than every `Queues` row's
  `from` disables `v`, whatever earlier allowance still matches;
- a version that cannot be parsed, or that no row covers, is disabled, with the reason.

### 3. Capability per action

A transport is chosen per action, not per tool:
- **idle nudges** take only typed submit, under its evidence;
- **failed-turn retries** take typed submit under its evidence, or, for Codex, the native
  `turn/start` route.

`transport_for` becomes `transport_for(tool, action)`, and an episode's kind selects
the action. On the native route, Codex retries are enabled while Codex idle nudges stay
disabled. The native route needs no typed-input evidence: it never interrupts, and it
starts a turn only on the exact failed turn.

### 4. Every transport returns to the loop to be authorised

Every enabled transport makes its mutation only after the owning monitor re-authorises
it, as the typed send does now. The asynchronous preparation (pane and screen for
typed; thread and socket for native) hands back to the loop. The loop runs the same
last look: the live gate, the claim, session, role and work from disk, suppressors, and
the record re-read. Only then is the mutation started.

For native, the mutation is `start_after`'s `turn/start`, which itself re-checks that the
failed turn is the thread's latest. An uncertain submission is `Indeterminate` and is
never retried through another transport.

### 5. Version discovery, bounded and off the loop

The installed version comes from a bounded probe (3 s), run off the loop once per
executable, and cached:
- the executable is the one the agent is launched with (its launch config's program,
  resolved on the agent's own launch `PATH`), not the monitor's;
- a probe that hangs, fails or prints nothing parseable caches that reason, publishes it
  in the agent's state, and is not retried until the monitor restarts;
- until a probe answers, the harness is treated as not yet known: status-only for that
  agent, with no effect on any other agent's delivery or scheduling.

### 6. Run it (operational, not a review gate)

After `cargo install`, the master retires this repo's monitors that predate the install,
stopping their producers first, and reports the observed results to the user. Review is
of the committed evidence, implementation and tests, not of these steps.

## Acceptance

- **Per action.** A test drives the production selector with the committed table: Codex
  failed-turn retries are enabled, by evidence or the native route, while Codex idle
  nudges follow only typed evidence. The idle and failed branches are tested for both
  harnesses, under the outcomes actually committed.
- **Enabled.** For a harness with `Queues`, an idle agent owing work is nudged after the
  settle period, and a failed turn is retried on its class budget. Both go through the
  production `transport_for`, with no test-only `enable`.
- **Disabled.** For a harness with `Aborts`, nothing is typed, and its state carries the
  reason.
- **Versions.** Tested cases:
  - below `from`: disabled;
  - at `from`: enabled;
  - beyond `from`: enabled by policy, with the policy named;
  - an `Aborts` row at or below the version revokes it, though the earlier allowance
    still matches;
  - at the revocation cutoff: disabled;
  - an unparseable version: disabled, with its reason.
- **Probe.** A hung probe, a failing one and an unparseable one each publish a reason,
  are not retried, and do not delay another agent's delivery or the scheduler.
- **Authorisation after preparation, on both branches.** A change in eligibility during
  preparation vetoes the mutation: the claim, session, role, work, auto mode, a
  question, block or attendance, a parked deliverer, or a newer turn. An uncertain
  native submission is never retried through typed input, nor the reverse.
- **The draft race** is stated in each committed evidence row.

## Out of scope

OpenCode and Grok, which stay as they are.

## As built

- **Observed (2026-10-13), in a background zellij session, scratch directories, each
  harness launched bare:**
  - **Claude Code 2.1.280 queues.** `write-chars` plus CR during a running
    1500-word-story turn showed "Press up to edit queued messages"; the story ran to
    `FIRST-DONE`, then the queued line ran.
  - **codex-cli 0.161.0 steers on Enter.** The same line sent with Enter (its kitty
    submit) was steered into the running turn; the model answered it and never wrote
    the story. That is the turn's work derailed, so Enter is never used.
  - **codex-cli 0.161.0 queues on Tab.** With Tab instead, it showed "Queued
    follow-up inputs"; the story ran to `FIRST-DONE`, then the queued line ran. Idle,
    Tab submits at once.
  - So both harnesses have a `Queues` row (`resume::EVIDENCE`, with the steps and the
    draft race), and Codex's typed submit is now `write-chars`, 200 ms, then Tab
    (`pane_input::codex_queue_argv`).
- **Version policy** (`resume::resume_in`):
  - each row has `from` (observed) and `later` (the forward policy);
  - among the rows at or below the version, the latest `from` decides, and an
    `Aborts` wins a tie;
  - unknown and unparseable versions are refused, each with its reason;
  - the agent's state says `observed to queue at X` or
    `allowed by policy beyond observed X`.
- **Per action.** `recovery::transport_for(tool, Action, version)`:
  - a nudge (idle or blind) takes only typed submit;
  - a retry takes typed submit, or for Codex otherwise `CodexStart`;
  - each episode selects by `Episode::action`, and the hint text follows the action.
- **One authorisation for every transport.** A transport prepares off the loop:
  - typed: pane and screen;
  - native: thread, socket and preflight, which finds the failed turn still latest.

  It then asks the loop through `Authorize` (`monitor::authorized`), with the repo
  folded afresh. The loop answers with `may_send_now`: the live gate, the composer for
  typed, the role and work from disk, and the driver's claim with the record re-read.
  Only on its yes does the transport write; a no answers `Unavailable` and sends
  nothing. `codex_interrupt::start_if_failed` takes that answer as a future, awaited
  after its preflight and before `turn/start`. Its old binding-only `still_bound` check
  is gone, since the loop's look covers it.
- **Version source.** The version that judges a capability is the one the agent's
  provider process was launched at, never a record's or a file's found later:
  - Codex writes `cli_version` only when a session is created, and a path probed later
    names whatever file an upgrade has put there under a running process;
  - so `clank agent start`, just before it execs, resolves the program from the
    launch's own directory on its `PATH` and stats the file;
  - it asks that file `--version` in the launch's own directory and environment
    (`agent::launch_context`, shared with the exec), stats it again, and accepts only an
    unchanged file that answers as its harness (`2.1.280 (Claude Code)`, `codex-cli
    0.161.0`; a runtime or wrapper's version is refused);
  - the probe runs in its own process group, its output drained on a thread, and one
    3 s budget covers both its exit and its output closing; the group is killed before
    the child is reaped, so a descendant holding the output cannot hold the launch;
  - the launch then execs the very file it resolved and asked;
  - the result, or why there is none, is written into the launch record beside the pid
    and birth that process becomes (`harness_identity::at_launch`,
    `pane_target::record`);
  - `harness_identity::identify` reads it back through that verified record
    (`pane_target::discover_process`).

  The remaining window is a file replaced between the probe and the exec a moment
  later. A record written before this change carries no version: such an agent is not
  typed into until clank relaunches it.
- **Identity lifecycle.**
  - An episode records the launch it was opened under (`Episode::bound`: the
    session and when it was bound), as it records its role. `decide` keeps it only
    while that is the agent's launch. So a new launch, even of the same session,
    retires it whenever it is applied: at the fold that sees it, at the end of a hold
    that deferred it, or on a restart after it happened offline. A restart under the
    same launch keeps the episode, its budget, latches and deadline as they were.
  - The final authorisation requires the episode's launch to be both the one the
    driver last saw and the one the agent's config states at the send, so a relaunch
    already on disk, or one whose retirement cannot yet be stored, refuses the old
    claim at once.
  - Attempt numbers continue past the old ones, and pending history settles on its
    own.
  - An episode stored before launches were told apart has no launch, and is kept as
    it is, its spent attempts and latches with it.
  - The identity is kept for the agent's whole binding (the `Session`, its launch time
    included) and for a process whose birth still holds. A relaunch, even of the same
    session, or an ended process, asks again once. A late answer for an earlier binding
    is dropped.
  - While an identification is pending, an episode refused only for want of its version
    (`NO_VERSION`) is held, never escalated: a deadline that has not come stays, one
    that has is dropped. The answer decides again from the current record, keeping
    spent budgets, attempt numbers and latches.
  - A final unknown answer is the documented disabled outcome, with its reason.
- **The final authorisation re-checks the capability.** After the record is read
  afresh, `still_permitted` requires three things:
  - the transport the harness running now permits for this action is the one the
    attempt was claimed with;
  - the requesting transport is that same one;
  - for a typed send, the pane's process is the identified one and still living.

  An obsolete claim is refused (nothing sent) and never sent another way. A later
  decision chooses afresh through the same owner. This holds for the blind fallback
  too.
- **The incident agent.** This repo's codex was launched before launch records carried
  a version, so it cannot be identified: it is not typed into, and says why. Its
  capacity failures are retried by its own `turn/start`. Relaunched by clank on 0.161.0
  or later, it is nudged by Tab.
- **Mutation-checked:** 9 mutants, all caught:
  - Codex submitting on Enter;
  - a revocation tie keeping the allowance;
  - later versions never allowed;
  - a nudge taking `turn/start`;
  - `turn/start` skipping the loop;
  - a transport ignoring the loop's no;
  - a claim outliving its capability;
  - a request using another transport;
  - a typed send trusting any process.
- **Pre-existing:** `git_boundary` fails on the base too (`stop_hook.rs` test code and
  `grok_hook_tests.rs`).
- **Identity revision mutation-checked:** 7 more mutants, all caught:
  - any first word read as a Claude version;
  - a file swapped during the probe trusted;
  - a pending identity not a hold;
  - a pending identity escalating;
  - a same-session relaunch not asked again;
  - an ended process keeping its identity;
  - an old launch's answer taken.
- **Launch-boundary revision mutation-checked:** 3 more mutants, all caught: a relaunch
  keeping the old episode, the probe ignoring the launch context, and an unbounded wait
  for its output. The group kill is not mutated: deleting a kill guard is destructive.
- **Launch-origin revision mutation-checked:** 4 more mutants, all caught: an episode
  outliving its launch, a legacy episode dropped, and the send ignoring either the
  observed launch or the one on disk.
