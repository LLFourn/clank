# work-only-when-something-happens
# Long-lived clank processes wake only on events: no beat, no backstop poll, and the dirty walk runs only when the worktree changes

## Why

On 2026-11-11 the machine had 340 clank processes using about one core between them. 88 were `clank status --watch -j`, one per repo, started by clankstack. Sampling one idle watcher showed its time in gix's status walk (`gix_status::dirwalk`, `index_as_worktree`).

The watch loop (`status.rs`, `run_watch`) works like this:

- **The gate probe is not cheap.** Before rebuilding, it computes `input_signature`, which calls `working_tree_dirty()`, a full index-vs-worktree compare plus a directory walk. It runs on every loop turn, including turns where nothing changed.
- **Turns come from three places:** any worktree event (a recursive watch), monitor wakes, and a timer `min(60s, replacing.due_in())`.
- **`no-process-outlives-its-build` made the timer fire every 5s** (`upgrade::BEAT`) to look for a new install. So since that plan, every idle watcher walks its whole worktree every 5 seconds.

The same beat drives the TUI loop (`status_tui/mod.rs`: `wait.min(replacing.due_in())`), the parked stop hook (`stop_hook.rs`: `replacing.parked()`), and monitored waits (`wait.rs`: `replacing.parked()`, `sleep(replacing.due_in())`). The TUI also has 60s idle ticks (`IDLE_TICK`, `OVERLAY_IDLE`).

LLFourn, 2026-11-11: "please remove the poll. We should only need to do work when something actually happens."

## Build

1. **An install is an event.** Each process that watches its installation subscribes to a file watch on the installed binary's path (the path `build_stamp::Installation` already reads, and its directory, since `cargo install` replaces the file). A change there triggers the existing install check (`upgrade::Authority::look`). `upgrade::BEAT` and every timer derived from it are deleted:
   - `Mover::due_in`
   - `Replacing::parked`'s beat
   - the status and TUI wait caps
   - the wait's `sleep(replacing.due_in())`

   A failed exec's retry backoff stays, because that is a timer the process set for itself after an event, not a poll.
2. **No backstop poll.** The status watch's 60s `recv_timeout` and the TUI's `IDLE_TICK`/`OVERLAY_IDLE` go. They are replaced by:
   - **Wake only on events:** worktree, `.clank/`, git state, monitor wakes, the installed binary, terminal input, and any timer that is a real deadline (a displayed age or countdown that changes on screen).
   - **A displayed clock is a deadline, not a poll.** Name each one, and don't wake on a fixed tick for it.
   - **A watcher that errors or disconnects fails loudly** with a visible error, never silently.
   - **A path-less event (FSEvents' "must rescan")** still wakes, as now.
3. **One cached Git status, invalidated by exactly the events that can change it.** Removing the backstop makes the event set the only source of freshness, so the dependency boundary has to be complete:
   - **One result, every consumer.** The Git status result (`DirtyStats`, including the diff statistics) is computed once per invalidation and cached per repo. Every reader uses it: the gate probe (`InputSignature`), `StatusSnapshot::build` (which today walks again on its own, around `status.rs:688`), and the TUI and watch paths. Nothing else calls `working_tree_dirty()`. A changed event therefore walks the tree once, never once for the probe and again for the snapshot.
   - **What invalidates it:**
     - any change to a path Git tracks, including tracked files under `.clank/` (plans, finished, `.gitignore`) and force-added files under an ignored directory;
     - a new untracked file that is not ignored;
     - the index;
     - HEAD or the ref it points to, since staged changes are relative to HEAD (`git reset --soft` moves it with the index and worktree unchanged);
     - anything that changes what Git sees: `.gitignore` files, `info/exclude`, the repo's config;
     - a path-less "must rescan" event.
   - **What never invalidates it:** ignored and untracked churn (build output, Git-ignored runtime state under `.clank/`), monitor wakes, and install events. These still refresh the cheap fingerprints they belong to (`.clank` state, health, sources), and a refresh then reuses the cached Git status.
   - **The filter is fixed, not worked around.** `WakeFilter` today drops tracked-but-ignored edits and leaves them to the 60s backstop (its comment around `status.rs:1435`). It must tell tracked Git inputs apart from ignored or untracked churn by asking the index, not by path prefix or ignore pattern alone.
4. **The same rule for every long-lived process.** Find any other fixed-interval wake in the monitor, the waiters, the stop hook or the TUI that does work with nothing changed. Remove every one and list each in the As-landed notes. The only poll allowed is GitHub source polling (LLFourn, 2026-11-11: "only poll I will allow is polling GitHub, because that is one we apparently cannot fix"). A poll that seems to stand in for a fact with no event is not kept on that ground: find the event, or stop and raise it. LLFourn clarified later the same day: "I just don't know what polls we have. If there's some poll we need, then we have to keep it, but I don't know why we would need it. So please try and get rid of polls." So a poll is kept only after looking for its event and finding none, and the As-landed notes name each one with the fact it stands in for and why that fact has no event. A real deadline (a presentation's expiry, a check-in's `--expect`, recovery's next action) is not a poll.
5. **Docs:** the README's upgrade section says installs are noticed by watching the binary. Any "polls every…" wording about these loops goes.

## Out of scope

- Merging clankstack's 88 watchers into one process. That's a separate plan, a multi-repo `status --watch`.
- GitHub source polling. That is polling a remote service with no push channel, by design (`github-source-polls`).

## Tests

No harness, monitor or real binary is started. The watcher is behind its existing seam.

- **Idle costs nothing:** with no events, the status watch loop and the TUI loop do not compute the signature or walk the worktree, however much (paused) time passes. Count `working_tree_dirty` calls through a seam.
- **An install wakes them:** a change to the installed binary's path makes a parked hook, a monitored wait, the status watch and the TUI each run their install check once, and re-exec or show the badge as before.
- **The Git status cache follows its inputs:**
  - Each of these recomputes it once: an edit to a tracked file, a tracked `.clank/plans` file, a force-added file under an ignored directory, a new non-ignored untracked file, staging, a HEAD-only move (`reset --soft` to another tree), a `.gitignore` change, and a must-rescan event.
  - An ignored build-output write, an ignored `.clank/` runtime write, a monitor wake or an install event does not.
- **One walk per refresh:** counted across a whole refresh, not just the probe. A changed event walks once and the snapshot reuses it; a monitor-only refresh walks zero times.
- **A watcher error is loud:** a disconnected or erroring watcher ends the watch with an error, and the TUI shows it.
- **Displayed clocks still tick:** a row showing an age or a countdown updates at its own deadline with no other event.
- **Mutation:** restoring the beat fails the idle test. Calling `working_tree_dirty` in the probe or the snapshot again fails the one-walk test. Dropping HEAD from the invalidation set fails the HEAD-only test. Dropping tracked-but-ignored paths fails the force-added test.

## Done when

An idle repo's clank processes do no work until something happens: a file changes, the monitor wakes them, clank is installed, a person types, or a deadline they show falls due.

## As landed

### Removed fixed-interval wakes, and what replaced each

| Wake | Interval | Replaced by |
|---|---|---|
| `upgrade::BEAT` in status, TUI, stop hook and monitored waits | 5s | `Authority::subscribe`: a kernel watch on the install directory (and a link's target directory) and on the installed file's own writes, plus a wake when the last permit drops on a stale build |
| status watch backstop | 60s | events only, plus its own refresh-retry backoff after a failure |
| TUI `IDLE_TICK`, `OVERLAY_IDLE`, idle refresh | 60s | named clock deadlines (`clocks_change_in`, `View::changes_at`, `Attended::age_from`) and process-exit events (`ProcessExits`) |
| monitor `FOLD_TICK` (fold, serve, reconcile, probe) | 5s | repo changes, client arrivals/reports/departures, run exits (`RunExits`), installs, lease changes, and the deadlines it holds (timers, timer retry, recovery, turns, check-ins, user commands, waiter settle, quiescing end) |
| monitor `WATCHDOG_TICK` | 5s | `monitor_watchdog`, armed only while a turn is in flight, folds keep failing, or the monitor is retiring |
| client `BEAT_GRACE` silence detection, `forward_beats`, keeper `SUBSCRIBE_POLL` | 20s / 500ms | blocking reads; a lost monitor is its socket closing (`Hangup`, `next_note`) |
| `RepoStateWatcher` `PollWatcher`, `wait --poll`, `CODEX_SANDBOX` switch | 500ms | kernel watch (`proc_identity::tree_watch`); a watch that cannot attach is an error |
| self-folding waits (`--event`, `--for`, `--peek`) | 1.5s / 500ms | events, one self-wake after attach, and a timer-retry deadline |
| command follower `TAIL_INTERVAL`, GitHub follower tail | 2s / 60s | a kernel watch on the holder's log (armed before the first read) and the lease coming free |
| deferred-retirement retry | 2s | a member exit, or a backoff after failure |
| `WORK_LOOK` | 50ms | member exits (`watch_members`, arm then look again), or the stop-grace deadline |
| transcript `TAIL_POLL` and open loop | 500ms / 2s | a kernel watch on the transcript |
| status/TUI dirty walk on every turn | every turn | one `GitStatus` per process, invalidated only by a Git input |
| web bridge pane listing (`PANE_POLL`, 1s supervisor tick) | 4s | `Feed::relist`, asked by a subscribed pane closing, a subscription ending, the TUI's listing finding a different set of agents' panes or reopening one, the connector attaching, and a viewer verifying a pane the table does not hold as open; restarts and verification samples wait on their own deadlines |

### Still on a clock, and why

- **GitHub source polling:** allowed (LLFourn: "only poll I will allow is polling GitHub").
- **Deadlines owned by work:** recovery's next action, presentation expiry, check-in `--expect`, timers, the monitor watchdog's in-flight budget, a group's stop grace (`until_a_member_exits` sleeps to it only when no exit watch can be made).
- **Retries after a failure:** exec retry (0.5s→8s), fold retry (500ms→8s), timer arming (1s→60s), status refresh retry (1s→30s), `OUTAGE_RETRY_CAP`, bridge reconnect, keeper respawn, producer restart backoff.
- **`SPINNER_TICK`:** animation, only while a spinner shows.

### Supporting changes

- **One Git status:** `GitStatus` is invalidated by `WakeFilter::sees` returning `Seen::GitInput`:
  - tracked paths (the index decides, so tracked `.clank` files and force-added files count), unignored untracked paths, `.gitignore`;
  - files named by `status_inputs_elsewhere`, found from the config sources themselves (the standard levels plus every file an `include.path` or `includeIf.*.path` names, whatever its condition says now, resolved as Git resolves them, with Git's case-insensitive names, nesting up to Git's limit of 10 with cycles cut, no cap on how many, and deeper nesting a reported error, never a partial set), so an empty, comment-only or missing one counts; each followed by the path Git names (as a directory entry, so a symlink retarget is heard) and by the file it resolves to (content and replacement); the set is re-read until it matches what is registered, and the cache is told only after the new inputs are watched, so a newly included config or newly selected `core.excludesFile` is followed without a restart;
  - git-dir HEAD, index, refs, packed-refs, config and info files;
  - rescans.

  Ignored churn, `objects/`, `logs/`, locks, monitor wakes and installs never invalidate it.
- **What counts as a change:** `fs_notice` is the one rule for native notifications: reads and opens are not changes, a rescan is a conservative look-again, and an error is a failure that ends or degrades its owner loudly.
- **FSEvents** remains only for the status/TUI worktree watch, one stream per process, pinned by `fsevents_boundary`. Moving every other watch off it cut `fseventsd` load and restored the integration suite to about 55s.
- **Kernel watch:** `tree_watch` re-registers a replaced path by identity, and treats an inotify queue overflow as a rescan.
- **Process liveness:** `proc_identity::inspect::Life` reads it from process state, separately from identity: zombie, dead or absent is Gone, unreadable is Unknown and still counts as work.
- **Monitor startup:** a starting monitor forgets only its named predecessor's observation, not a listing of the shared runtime directory.

### Known limitations

- The web bridge, like the TUI (`the-tui-lists-panes-only-on-change`), hears no announcement of an agent pane opened, moved or renamed by hand: zellij has no layout event. It is listed at the next of the triggers above; the TUI's own listings (entering the agents screen probes) are one of them.
- An install that retargets a link into a directory not watched at subscribe is heard only through the link's own directory.
- Linux-specific paths are exercised only through portable fixtures. The first Linux run is CI.
- The Codex sandbox's reported loss of FSEvents gitdir events (`wfw-daemon-style-watches`) has not been retested. The gitdir watch now uses kqueue/inotify, not FSEvents.

### Validation

- The parallel `--lib --test-threads=4` runs skip `remote_owner`. Run in parallel, that group has a known timing failure (`applied_waits_for_the_transition_and_a_second_caller_is_busy`, and under heavier load others of the group); the whole 12-test group passes serially. No run here claims a wholly green parallel workspace run.
