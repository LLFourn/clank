# check-ins-go-through-the-wait
# An attendance check-in is a notice delivered through the agent's parked hook, never typed into its pane

## Why

On 2026-11-11 Codex's pane in the clank repo showed six queued follow-up inputs, one per `clank run` it had started for its reviews ("upgrade review", "reachability correction", …), each reading "clank check-in: the `clank run` … has been going for 5m …". None reached the model.

Each `clank attend` or `clank run` entry has an `--expect` (default 5m). Past it, clank checks the agent in "once in its pane where clank can type there" (`monitor::deliver_check_in`, the `Typist`). While Codex's Stop hook is parked, Codex shows the turn as Working, so typed text becomes a queued follow-up input that the model never sees until the hook returns. Each run fell due at a different time, so each typed its own message.

LLFourn, 2026-11-11: "Why do we have #2 at all. It certainly shouldn't be used for codex."

A parked hook can already return a notice with no work: the monitor-outage notices do. A check-in is one more notice. An agent with no parked hook is unreachable, which recovery already owns.

## Build

1. **A check-in is a notice.** When entries pass their `--expect`, the monitor presents one notice through the agent's parked hook, listing every entry due, in the existing check-in wording. It goes the way every other notice goes (`Verdict::Notify` and its wake rendering), with the same delivery, receipt and expiry as any presentation. A wake that is carrying work anyway carries the check-in beside the work.
2. **Typing is deleted.** `deliver_check_in`, `CheckInAsk`, the check-in use of the `Typist` and its last-look path go, and so does every call path that exists only for them. Nothing types a check-in into any harness's pane.
3. **When the agent isn't parked:**
   - Mid-turn: entries that fall due wait. The agent's next park delivers all of them in one notice. A turn that is working needs no check-in.
   - No waiter at all: the check-in waits too. The unreachable agent is recovery's job (`reachability-is-evidence-not-inference`); the check-in is delivered at the park that follows.
   - Mid-turn means any report that does not hold the agent's recorded Stop: a compaction or re-step whose record has moved past it (a turn a person started) carries nothing.
   - **Grok** is checked in at its native Stop, through the same presentation and result exchange its session start uses. Its background wait is not a carrier: nothing reports a Grok turn starting, so a check-in served to that wait could land mid-turn. A Grok agent idling on its wait therefore gets a due check-in at its next turn end, not the moment it falls due. Delivering it while Grok idles needs turn-start evidence from a new Grok hook, which is a separate plan.
4. **Once, still.** Each entry is checked in on once per `--expect` window, as now. Renewing it with `clank attend "<name>" --expect …` arms a new one. Entries due before one delivery share it.
5. **State:** `CheckIn::{Armed, Fired, Withheld, Undeliverable, ProcessGone}` is reduced to what a notice delivery can produce. A check-in owed but not yet delivered is shown as owed (`check-in due` on the attended row), not as fired.
6. **Docs:** the README's attending paragraph says the check-in comes through the wait at the next park, never by typing.

## Tests

No pane is typed into; no harness is started.

- **The incident:** six runs falling due at different times while the agent's hook stays parked. Each is delivered through the park as soon as it falls due (one notice if several fall due before a delivery), and nothing is typed.
- **Mid-turn:** entries falling due during an open turn are delivered together at the next park.
- **No waiter:** nothing is delivered and nothing is typed. The check-in stays owed, and the next park delivers it.
- **With work:** a wake carrying work also carries the due check-in.
- **Renewal:** renewing after a delivery arms one more check-in; renewing before keeps it unfired.
- **Restart:** an owed, undelivered check-in survives a monitor restart and is delivered once.
- **Mutation:** restoring the typed path, or delivering a check-in mid-turn, fails a test.

## Done when

No check-in is ever typed into a pane, and every check-in reaches the model as a notice at the agent's next park.

## As landed

- **A check-in is a presentation's notice:** `Notice::CheckIn(Vec<CheckInLine>)` on a `Verdict::Deliver`, with or without work beside it, in the existing wording; `Presentation.check_ins` carries the per-window `CheckInRef`s. Not `Verdict::Notify`, because a Notify has no result to spend it on. It takes the same delivery, result and expiry path as any presentation.
- **State:** `CheckIn::{Armed, Due, Fired}`. Every step moves an expired Armed entry to Due (`fall_due`). Only a Delivered result sets Fired, and only for entries still Due in the presented window (`still_owed`: `check_in == Due` and the window id), so a renewal or a stale result never spends a new window. Failed, closed, overdue or restart-lost presentations leave it Due.
- **Carriers, by which report custody serves:** `Stop` (a fresh TurnEnd), `Held` (`Event::Parked`, the very report the park holds, placed or not), `Compacted` (only on `Stopped::stands_in`; matching clocks, missing or contradicting placement never count), `NativeStop` (Grok's native Stop, beside `ArmWait` when no wait is registered), `SessionStart` (never). The idle background wait is never a carrier. `carried_check_ins` is the one gate for all of them.
- **Timing:** due entries batch into one notice. A parked agent gets it at once (`deadline_round` re-serves parks). Mid-turn or with no waiter it waits for the next eligible turn end. Grok gets it at its next turn end, not while it idles on its background wait, because nothing reports a Grok turn starting.
- **Grok's result:** `answer_once` hands its verdict to an `emit` callback and the hook answers with `result_of` once it has written, so Delivered means the flush succeeded.
- **Deleted:** the typed route (`deliver_check_in`, `CheckInAsk`, `CheckedIn`, `may_check_in_now`, the loop's channels and wake arms), `turn_custody::{CheckInDue, check_in, check_in_failed, may_type_check_in}`, core `Effect::CheckIn`, `Event::CheckInUndeliverable`, `check_in_due`, `check_in_undeliverable`, `may_type_check_in`, `is_fired`, and `recovery::Driver::may_type_check_in`.
- **Old bytes:** `Withheld(_)`, `Undeliverable(_)` and `ProcessGone` load as Due; `Unarmed` as Armed. A fixture pins an in-flight check-in presentation.
- **Shown:** `check-in due` / `checked in` on the attended row. The README and `book/how-it-works.html` say the check-in comes through the wait, never typed, with Grok's limitation.
- **Windows:** each entry's expectation is a `Window` with a persisted id, created only by a declaration, a run or a renewal. A `CheckInRef` names that id, and only a delivery of that id while it is Due spends it. Binding keeps the window; merging moves the pending window, with its check-in state, into the bound entry. Old entries get ids on load, and an old key-and-`since` reference names no window.
- **Left open:** `answer_with`'s emit-then-answer wiring has no direct test.
