# attendances-stack-and-show
# An agent attends several things at once, sees them in the TUI, and is woken whenever it stops with live work it is not attending

## Why

In one afternoon the master's attendances failed five times. `clank run`
had registered its own attendance correctly every time. The master then
ran `clank attend "<desc>"` as well, and that silently replaced the
working one:

- An agent has ONE attendance slot. A second `attend` overwrites the
  first, including the one `clank run` made for itself.
- `clank attend "<name>"` matches a task's `description`. A shell
  task's description is the Bash tool's description ("Run the full
  integration test harness"), never the `--desc`, so a shell job can't
  be attended by name. With `--pid`, the name has to be the task ID.
  Both mistakes are accepted at request time and only fail at the next
  turn-end.
- The hook's "Live now:" line lists shell work by that description,
  which invites the very `attend` that can't match it.
- The TUI shows an attendance only once it qualifies, so a broken one
  is invisible.

The user's rule, verbatim: "You should be able to attend multiple things
at a time. Show it in the tui. Clank wakes you if you're attending
nothing and end your turn." Also: "if you stop and its your turn you get
woken up and can only circumvent that by attending something (a
subagent or a process)."

## What changes

1. **Attendances are a set.**
   - An agent's attendance becomes a collection: `Attendance::None |
     Intent | Standing` becomes a list of entries, each with its own
     selector, proof, `--expect` deadline and check-in.
   - Adding one never removes another.
   - An entry ends on its own when its work leaves the listing, when
     its process proof fails, or when it is cleared.
   - `clank attend --clear` clears all entries. `clank attend --clear
     <name>` clears one.
   - The persisted turn state migrates. An old single `attendance`
     reads as a one-entry set, and old files keep loading. Fixture the
     old bytes.

2. **One way to attend each kind of work.**
   - A subagent is attended by its name: `clank attend "<name>"`, as
     today.
   - Shell work is attended only through `clank run --desc "…" --
     <cmd>`, which registers itself, as today.
   - Remove `clank attend --pid` and the task-ID selector. `clank run`
     covers them.
   - A name that matches only shell work is refused at the turn-end that
     resolves it, with a direct fix: "`<name>` is shell work; shell work
     is attended by launching it with `clank run --desc …`". The
     refusal does not touch any other entry.

3. **The turn-end rule.** At a Stop, an attendance that qualifies holds
   only its own work. Then:
   - **Live work, nothing attended.** The listing shows live background
     work, and no entry covers it: no subagent named and no `clank run`
     matching it. The agent is woken with a notice naming each
     uncovered task and how to attend it, even when no other work is
     owed. That is what "attending nothing" means: stopping with live
     work clank isn't watching.
   - **Already-shown work stays quiet** only while at least one entry
     qualifies, as today, and the attended task's own completion still
     wakes the agent.
   - **Nothing live and nothing owed** parks the agent quietly, as
     today. No loop.

   The notice lists shell tasks by the `clank run` that covers them, or
   says "not attended — relaunch with `clank run`". It never lists them
   by their Bash description.

4. **The TUI shows every entry**, on the agent's row and its detail
   page:
   - qualified entries, with what they cover and their expected end;
   - entries that have not qualified yet, with the reason (not seen in
     a listing yet, shell work named by description, past its deadline,
     process gone).

   An agent attending three things shows three.

5. **Docs.** Update the master, reviewer and standby skills'
   instructions for waiting on background work, the README section on
   `attend` / `run`, and the hook text in `wake_render.rs`. Each says:
   subagents by name, shell work by `clank run`, and that several can
   be attended at once.

## Contracts (from review of the intro)

### Coverage is per task; quieting stays as it is

- **An entry covers exactly the task it resolved to.** Coverage is the
  union of the task ids that qualifying entries cover, decided per
  task.
- **Mixed case.** With A attended and qualifying and B live and
  uncovered, the Stop wakes with a notice naming B. A's own coverage
  is unaffected.
- **The receipt mask stays global.** Already-shown generic owed work
  stays quiet while ANY entry qualifies, as today. NEW owed work still
  wakes. This is a separate mechanism from task coverage, and the two
  are never merged.
- **Which tasks the notice may name.** Only tasks that a permitted
  command could cover:
  - `TaskKind::Subagent` and `TaskKind::Shell`, in an available
    (fresh, known) listing.
  - Only for tools with `completion_wake` (Claude today). Other tools
    get no uncovered-work notice, exactly as attendance quieting is
    Claude-only today.
  - Excluded: `TaskKind::Unknown` (monitors and the like), a
    `TaskSnapshot::Unavailable` listing (it proves neither liveness nor
    completion, so nothing is named), and clank's own internal waiters
    (a shell task whose command parses as `clank wait …`, such as
    Grok's handoff). A healthy idle harness therefore has no task to be
    told about, and cannot loop.
- **Gates.** The notice goes through the same gates as any wake: auto
  off, a question pending, blocked, stale binding. A gated Stop sends no
  notice.
- **Repeats.** A notice repeats on each Stop while the task stays
  uncovered, which is the user's rule ("can only circumvent that by
  attending"). It uses the existing repeat and escalation counting
  instead of a new loop.

### Entry identity, and mutations scoped to it

- **Entry keys.**
  - A subagent entry is keyed by its name until it resolves, then by
    its resolved task id.
  - A run entry is keyed by its process proof (pid plus birth token).
    `--desc` is only for correlation and display.
- **Re-attending a subagent renews in place.** Attending a subagent
  that already has an entry, by the same name or by a name resolving to
  the same task, renews that one entry: a new `--expect` deadline and a
  re-armed check-in. No duplicate is appended. Other entries' clocks
  are never touched.
- **Two runs may not share a `--desc`, in the listing either.** A run
  whose `--desc` is already used by a run entry still in the set,
  resolved or not, gets a distinct `Refused::DescInUse` answer from the
  monitor.
  - On that answer `clank run` does NOT exec. It exits non-zero with
    "another `clank run` is already attended as `<desc>`; give this one
    a different --desc".
  - So no second task whose command parses to that desc ever enters
    the listing, and the existing entry stays uniquely correlatable.
  - Only "no monitor took the request" still runs the command
    unattended, as today.
  - Once the earlier entry has genuinely left the set, the desc is
    free again.
- **A failed exec removes only its own entry.** `clank run`'s rollback
  sends a clear scoped to the exact entry it registered, by its process
  proof. An unqualified clear-all is reserved for the explicit
  `clank attend --clear`.
- **`clank attend --clear <name>`** removes the entry whose subagent
  name or run `--desc` equals `<name>`. If both a subagent entry and a
  run entry match, it is refused and both are listed, so the operator
  can pick.
- **Results are entry-scoped.** A check-in result, completion, refusal
  or proof failure applies to its own entry key. A result arriving for
  an entry that has since been renewed (a newer deadline) or removed is
  discarded. It never touches a different entry, and never consumes
  another entry's check-in.

### Deadlines and check-ins

- The monitor's next wake is the nearest deadline over all entries. No
  per-entry polling.
- Check-ins that come due together give ONE check-in wake listing each
  due entry. Each listed entry's check-in is consumed; no other entry's
  is.

### Lifecycle and storage

- **An entry leaves the set when:**
  - its task is absent from a fresh known listing, meaning the work
    ended and the provider's completion wake reports it;
  - it is cleared;
  - its exec failed.
- **Process gone.** A run entry whose process proof fails while its task
  is still listed stays as an UNQUALIFIED entry, with reason "process
  gone", so the TUI can show it. It leaves at the first fresh listing
  without its task.
- **Storage is bounded.** Stored entries are bounded by the current
  live and recently requested work. Ended entries are never
  accumulated.

### Migration of the persisted turn state

- Old `None` loads as an empty set.
- `Intent(req)` loads as one unresolved entry.
- `Standing { … renewal: None }` loads as one standing entry.
- `Standing { … renewal: Some(req) }` has two cases:
  - if the request names the same work, it loads as that entry with
    the renewal pending;
  - if it names different work, it loads as the standing entry plus an
    unresolved entry for the request.
- Restored entries still need a fresh listing (and, for a run, a fresh
  process proof) before they quiet anything.
- Each shape is a byte fixture of the OLD format.

## Out of scope

- No change to how work items are owed, delivered or deduped.
- No new process supervision. `clank run` keeps its exec design.

## Acceptance

Covering the contracts above as well:

- **Mixed:** with A covered and B uncovered, the notice names B only,
  and the receipt mask still quiets shown work. New owed work wakes.
- **Eligibility:** no notice for a monitor task, a `clank wait` shell,
  an unavailable listing, or a non-Claude tool. A gated Stop (auto off,
  asking, blocked, stale) sends no notice.
- **Scoped rollback:** two valid entries plus a third run whose exec
  fails leaves both entries, with their deadlines and check-ins intact.
- **Renewal:** re-attending subagent A renews A's deadline only. B's
  deadline and check-in are unchanged, and the set still has two
  entries.
- **Same desc, end to end.**
  - With run A registered but not yet resolved, run B with the same
    `--desc` is refused with `DescInUse`, and B's exec closure is never
    called.
  - The next Stop's listing, containing only A's task, resolves A,
    with its deadline and check-in intact.
  - The same holds when A is already resolved.
  - After A's entry ends, a new run with that desc is accepted.
  - "No monitor" still execs unattended.
- **Ambiguous clear:** `--clear X`, where X is both a subagent name and
  a run desc, is refused, listing both.
- **Stale results:** a check-in result for an entry that was renewed
  meanwhile is discarded.
- **Check-ins:** two entries due together give one check-in wake
  listing both.
- **Process gone:** the entry is shown with that reason, then removed at
  the next listing without its task.
- **Migration fixtures:** None, Intent, Standing with no renewal,
  Standing with a same-task renewal, and Standing with a different-task
  renewal.

Tests drive `turn_policy` and the monitor's control handling
in-process. They never spawn clank, claude or a monitor.

- **Stacking:** a `clank run` entry plus a subagent entry both qualify
  in one listing. Ending one leaves the other qualifying. A later
  `attend` of the subagent does not disturb the run's entry.
  - Mutation: make `attend` replace the set, and this test goes red.
- **Shell named by description:** refused with the `clank run` fix
  text, and the existing entries are unchanged.
- **Live work, nothing attended:**
  - A Stop whose listing has a live shell task with no covering entry,
    and no owed work, gives a wake that names the task. With the task
    covered by a `clank run` entry, the same Stop parks.
  - With an empty listing and no owed work, it parks.
  - Mutation: drop the uncovered-work wake, and the first case goes
    red.
- **Migration:** a turn file holding the old single-attendance shape
  loads as a one-entry set and keeps qualifying.
- **TUI:** an agent with two qualified entries and one unqualified
  entry renders all three, with the unqualified one's reason. Snapshot
  the row and the detail page.
- **Removed flag:** `--pid` does not exist; clap rejects it with no
  test of ours. The kept path, `clank run`, is tested.
- Run `turn_policy`, `turn_custody`, `attend`, `run`, `wake_render`,
  `monitor` and `status_tui` tests, then the whole `-p clank --lib`,
  the `cli::command` README tests, clippy and fmt.

## As landed

The plan landed as written. These details differ:

- **Re-attending takes effect at the next listing, not at request time.**
  A name request renews nothing immediately. At the next fresh listing:
  - if the name still resolves to the same task, that entry is renewed
    in place;
  - if it resolves to a replacement task, the request attends that
    task, and the old entry ends with its task.

  A name equal to a run's `--desc` renews that run only at a listing
  where no live subagent carries the name. Run renewal by `--desc` is
  how a run past its `--expect` gets more time.
- **The task-id selector survives internally for migration only.**
  Legacy `--pid` intents and renewals load and need fresh listing and
  process proof before they qualify. The monitor refuses the selector
  on the wire, and there is no CLI flag.
- **Uncovered demands use the counted delivery lifecycle.** They are
  presented as `uncovered:<task>` identities, repeat and escalate like
  owed work, and never enter the received-work mask.
  - A demand requires a usable name that resolves to that task alone.
  - A reopened wait or a session start presents only owed work.
  - A person's waiting signed command also gates the demand.
- **A grouped check-in is rendered at its last look**, from the entries
  still owed one.
- **Any monitor refusal now stops `clank run` before exec**, not only
  `DescInUse`. Only "no monitor took it" still runs unattended.
- **`clank status` JSON `attending` is a list** (`[]` where it was
  `null`).
