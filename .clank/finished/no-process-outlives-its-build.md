# no-process-outlives-its-build
# After an install, every clank process on the delivery path moves itself to the new build, and every display process either does too or says it's stale

Third of four plans from one diagnosis (LLFourn, 2026-11-10: "What is the problem with our architecture … No refactor is too ambitious"). It follows `the-monitor-alone-decides-a-wake` and `undelivered-work-is-noticed`; `reachability-is-evidence-not-inference` comes after.

## Why

clank is several long-lived processes: the monitor, each agent's Stop hook and its `clank wait --turn-end` child, Grok's waiter, `clank tui`, and `clank status --watch`. Only the monitor reacts to an install: it retires when a different build is installed (`monitor.rs`, `Settled::Stale`). Everything else keeps running the code it started with, against a newer monitor and newer files. Three bugs in one week came from that:

- **A stranded waiter (10 Nov).** `artist4`'s waiter, started 9 Nov 16:30, couldn't follow the monitors that replaced its own through four reinstalls and a wire protocol change (19 → 20). Its message was never delivered.
- **"monitor not running" (10 Nov).** A TUI on protocol 19 refused a protocol-20 monitor's source account, and reported a running monitor as absent while GitHub delivery was down.
- **Invisible attendance (10 Nov).** A TUI from before `1fdebde8` read `"subagent": true` to draw a declared entry. The new build wrote `"declared": true`, so the entries vanished.

Each was patched where it showed (lenient readers, a serde alias). Patching pair by pair can't end, because every release adds a new pair. Making upgrade the same path as restart ends it: a process whose state is all on disk can replace itself at any safe point.

## Build

1. **One check, shared.** Factor the monitor's own "is a different build installed" test (`build_stamp::Installation`, used by `Built`) into something every long-lived process can ask. It's checked:
   - on a beat;
   - whenever a monitor is lost or an attach is refused;
   - before writing shared durable state.
2. **Delivery-path processes replace themselves.** The Stop hook with its turn-end child, and Grok's waiter, re-exec into the installed binary when the check says so, at a safe point: parked, with no presentation in flight and no result owed.
   - `exec` keeps the pid and the inherited pipes, so the harness sees nothing.
   - State the process holds only in memory is handed across explicitly: the hook input and the report already consumed from stdin, through an inherited descriptor or a file named in the new process's arguments. It's read back by a fixture-tested format.
   - The hook and its child move together, or the frame between them becomes a fixture-pinned contract. Pick one and say why.
   - A re-exec that fails (binary missing mid-install, exec error) leaves the old process running and retries with backoff. It never exits and never parks silently.
   - This replaces the "upgraded; ending this wait" notice that `the-monitor-alone-decides-a-wake` added for a monitor of another protocol: no model turn is spent on an upgrade.
3. **A stale process stops writing.** A process that has seen a newer build installed writes no shared durable state (turn state, event logs, agent state, source accounts) between that moment and its re-exec.
4. **Display processes converge or say so.** `clank tui` and `clank status --watch`:
   - re-exec the same way when nothing they host would be broken by it;
   - never re-exec while hosting a live remote session or tunnel, because a restart there drops LLFourn's connection, and that is a standing rule. Such a process shows `newer clank installed · restart to upgrade` in its bar instead;
   - never silently misread: a file or account written by a newer build that this build can't interpret is shown as such, not as absent.
5. **The monitor is unchanged in kind:** it already retires. Its retire and a waiter's re-exec must compose: a new waiter that finds an old monitor waits for its retirement and spawns the installed one.
6. **Docs.** The README's upgrade section says what happens to running agents on install: nothing visible; waiters move themselves; TUIs restart or say they're stale.

## Out of scope

- Keeping the wire protocol compatible between builds. Detection is by the installed build stamp, so no cross-build wire contract is needed beyond "an unknown hello is refused".
- Restarting agents' harness sessions.

## Tests

No test execs clank or starts a harness. The exec is behind a seam.

- **The decision:** installed stamp differs and the process is parked with nothing in flight → re-exec requested once. Mid-presentation → deferred until the result is forwarded. Same stamp → nothing.
- **The handoff:** the consumed hook input and report round-trip through the handoff format. Fixture the bytes.
- **Failure:** a failing exec leaves the process parked and retried; nothing is emitted to the harness.
- **No stale writes:** after the check trips, an attempted write of each shared file kind is refused or deferred.
- **Composition:** a new waiter with an old monitor ends attached to a monitor of the installed build.
- **Display:** a TUI hosting a remote shows the stale badge and does not re-exec; one hosting nothing re-execs.
- **The three bugs as regressions:** the waiter after N installs is attached to the current monitor; the TUI on another build either converges or shows the stale badge, and never "monitor not running" for a running monitor; a newer attendance shape is never dropped silently.

## Done when

Within one beat of an install, nothing on the delivery path is running the old build, apart from a process mid-delivery, which moves right after. A display process that hasn't moved says so on screen.

## As landed

- **One authority** (`upgrade.rs`): `Authority` wraps `build_stamp::Installation`, one per process. `main.rs` arms it only for `Command::outlives_an_install()` (monitored `wait`, `stop-hook` except `--session-start`, `tui`, `status`). `Standing::{Current, Quiescing{in_flight}, Replaceable}`: a process is replaceable exactly when another build is installed and no `Permit` is out. The monitor's `Built` wraps the same authority.
- **Hook and child move together** (coordinated replacement, not a pinned frame): the parked stop hook writes a `Handoff` (`{v, input, report}`, fixture `hook-handoff.json`) and execs with `--handoff <path>`; the exec closes the child's pipes, so the frame between them is always one build's. The turn child never execs. Monitored waits (Grok's included) exec with their own argv. `Mover` retries a failed exec with backoff (0.5s to 8s) against whatever is installed then; nothing reaches the harness.
- **The barrier:** every persistence helper begins at `upgrade::writing()` or takes a `&Permit`; every action seam (pane input, native Codex RPC, interrupts, recovery dispatch, check-ins, zellij pane changes, git mutations) asks before anything reaches a harness. The one never-refused path is settling a delivery that already reached its agent. `tests/it/stale_write_boundary.rs` fails on an undeclared writer and pins the action seams by name.
- **Monitor:** quiesces in-flight presentations before retiring (`QUIESCE_BUDGET`), which the plan called "unchanged in kind". A stale monitor answers no report, sends no nudge, advances no feed cursor or CI horizon, and presents nothing it could not log. A nudge already sent holds its permit until its answer is recorded.
- **Retirement is not an outage:** an unattached wait reads retirement from the repo (lease held, or a live monitor of another build published) and gets `RETIREMENT_BUDGET`, composed from the lifecycle constants; a genuine failure keeps `OUTAGE_BUDGET`. A report that cannot wait out a retirement ends with a diagnostic, not a notice.
- **Display:** the TUI and `status --watch` look ahead of their rebuild gates. A TUI hosting a remote, or with an overlay or text input open, never execs and shows `newer clank installed · restart to upgrade`; an action it refuses says so in an overlay, and a remote ask gets the same wording. A source account or `state.json` from another build reads as unreadable, never as absent.
- **Deleted:** `Notice::Upgraded`, `monitor::Unserved` as it was, `wake_render::UPGRADED`, the wait's "clank was upgraded" bail.
- **Deviations:** self-folding waits (`--event`, `--for`, `--peek`) are not armed, since an exec would end their producers. A newer monitor's refusal with no newer install visible ends as an ordinary outage after 60s.
- **Left open:** the remote door's login-session store is ungated, because it is what keeps the live connection. The leaf pane and native-RPC functions are gated and pinned by the source gate but have no direct stale test. The old turn child is a zombie until the hook exits.
