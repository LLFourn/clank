# the-tui-lists-panes-only-on-change
# A clank TUI asks zellij for its pane listing only when something changed, and its spinner redraws at 2 fps

## Why

Measured 2026-10-16 on the `clank-sp-channel` zellij server: 55 panes, about 17 `clank tui`s, and the server at about 63% of a core.
- Each `list-panes --json --command` against that server costs zellij roughly 50–65 ms of CPU and returns about 61 KB.
- Every TUI lists:
  - on a 5 s timer (`status_tui/zellij.rs` `PRESENCE_PERIOD`);
  - and on every refresh, because `update_glyphs` is sent each refresh and any glyph batch forces a listing (`listed = probe || glyphs.is_some()`).
- That is about 3.4+ listings a second, roughly 20% of a core, for a session where nothing changed.
- The spinner frame (`SPINNER_TICK`, 120 ms) adds about 8 redraws a second per spinning TUI, which zellij parses and every attached client repaints.

Zellij 0.46 has no CLI subscription to layout changes: `subscribe` streams pane content only, and layout events are plugin-only.

## The observation contract

**Presence and session liveness are different facts.** A claude session can outlive its pane: Claude Code keeps it running in the background (`session_holder.rs`), and `lost_labels` shows exactly that state. So a bound session's process cannot announce a pane closing. Something that lives exactly as long as the pane must.

**The pane's own process is that signal.** `clank agent start` runs inside the pane. Just before exec it writes `pane_target`'s launch record: session, pane, verified `pid` plus `birth`, and tool. Because the agent is exec'd, that pid IS the pane's process, i.e. the claude or codex client. In both cases below, that process ends:
- **The pane is closed.** Zellij hangs up its terminal. A claude session that survives moves to a different, backgrounded process; the client attached to the pane still ends.
- **The agent exits in the pane.** The pane is held as exited, or it closes.

So the end of the recorded pane process is a complete announcement of "this agent's pane changed". Its continued life is proof that the pane is still there.

**What this does not see.** Two changes are not announced:
- a pane moved or renamed by hand;
- a pane opened for an agent by something other than `clank agent start`, which therefore wrote no launch record.

Neither changes presence for a recorded pane. Both are picked up by the next listing that some other event triggers. The user gets a fresh observation on demand by opening the agents screen, which already sends `probe()` (`status_tui/mod.rs`, "Entering the agents' screens asks for a fresh listing"). The status line does not claim freshness beyond this.

## What to build

### 1. A native process-exit watch

New `proc_identity::exit_watch`. It is backend-agnostic for callers and owned by the reconcile worker for the worker's lifetime.

**Input.** A verified identity `(pid, birth)` from a launch record. Watching first re-checks the birth, natively, with no subprocess. The check gives one of three answers:
- **Ended:** the pid is gone, or now has a different birth.
- **Unknown:** the inspection itself failed (permission, or an unsupported platform).
- **Watching:** an exit notification is installed.

**Mechanism.** Native exit notification, with no polling:
- **macOS:** one kqueue holds an `EVFILT_PROC`/`NOTE_EXIT` registration per pid, plus an `EVFILT_USER` used to wake it for rearm and shutdown.
- **Linux:** `pidfd_open` per pid, plus an eventfd, in one epoll.

**Threading and lifetime.** One thread blocks in that wait and sends `Exited(label)` to the worker's channel. Dropping the watch wakes it with the user event and joins it. No watcher thread or fd outlives the worker.

**Identity across registration.** Before registering, the watch checks that the pid's birth is still the recorded one. After registering, it checks again. A pid can be reused between the first check and the registration, in which case the kernel would be watching the replacement process. So:
- if the birth differs after registering, the registration is removed and the expected identity is reported Ended;
- a registration that fails with ESRCH is Ended;
- an inspection that fails at either check is Unknown.

Tests drive this ordering through an injected registration/identity seam. They do not try to force real pid reuse.

**Results stay distinct.** Proven exit and `Unknown` are separate results. Unknown never marks a pane missing. It shows the agent's presence as the last listing said, and that agent is re-listed whenever any other event triggers a listing.

**Notifications carry identity.** A notification is `Exited(WatchKey)`, where `WatchKey` is `(record path, pid, birth)`, not a bare label. The worker acts on it only if that key is still the current watch for that record. An exit queued from a watch that was replaced, or dropped, before the message was read is discarded.

### 2. Which records are watched, and when

**Records carry their label.** `pane_target::Launch` gains `label: Option<String>`, written by `record` from now on. It is `#[serde(default)]`, so records written before this change still parse, with no label.

**Selection rule.** The watch set is every launch record that:
- has this repo's `<repo_tag>.pane-launch-` prefix;
- carries a label;
- names this TUI's own zellij session (`ZELLIJ_SESSION_NAME`).

Old records without a label, and records for other zellij sessions, are never watched. Their agents' presence comes from listings triggered by other events, as today. Restarting such an agent through clank writes a labelled record.

**One key per record.** A record's filename already hashes label, session and pane. So one record is one pane, and its key is its path plus its `(pid, birth)`. Two panes for one label are two records, and both are watched; presence itself still comes from the listing. A relaunch into the same pane overwrites that record with a new `(pid, birth)`: a new key, which replaces the old watch.

**Terminal observations are remembered.** The worker keeps the last outcome per key. A key that has reached Ended, or Unknown, is never re-armed and never re-reported. Records are retained after their process exits (`pane_target` has no exit cleanup), so without this rule every listing would re-arm a dead identity, report it Ended at once, and trigger another listing, forever. A key's remembered outcome is dropped only when its record is removed or rewritten with a different identity. That keeps the memory bounded by the records on disk. Each identity therefore causes at most one exit-triggered listing.

**When the set is re-derived:** at TUI start, and on every record-set change. Not after listings: a listing changes no record.

**Record changes.** Records live in the per-user monitor runtime dir as `<repo_tag>.pane-launch-<hash>.json`, and today the TUI does not watch that dir. The watch thread from section 1 also watches the directory itself, through the same kernel interface: `EVFILT_VNODE`/`NOTE_WRITE` on the dir's fd in the same kqueue on macOS, and an inotify fd in the same epoll on Linux. It does not use `notify`. `repo_watch.rs` records that `notify`'s FSEvents backend costs about 10 s per `.watch()`, and `PollWatcher` would be polling again. On a directory wake, the thread re-reads only this repo's `<repo_tag>.pane-launch-*` entries and compares them with the last set. Only a changed set sends a record-change batch, so other repos' churn in the shared dir costs one readdir and never reaches the worker.

The worker never scans the process table: no `ps`, no `claude agents`.

### 3. List only on a reason

`worker_loop` no longer probes on a timeout; it blocks on its channel. A batch lists panes only when it carries one of these reasons:
- an `Exited` from the watch;
- a launch-record change;
- a roster change;
- a reopen;
- an explicit `probe()`;
- glyphs that differ from the last glyphs a listing was made for.

`lost_labels` keeps its current evidence path (`ps`, `claude agents`), but runs only in such a batch, and only for bound agents the listing shows without a pane. It never runs on a timer.

### 4. Glyph dedupe

The worker keeps the last `StatusGlyphs` it listed for, and a batch whose glyphs are equal to that does not list. A refresh with nothing new costs nothing in zellij.

### 5. Spinner at 2 fps

`SPINNER_TICK` becomes 500 ms.

## Acceptance

All tests drive the worker through a fake `PaneIo` that counts snapshots and a fake exit-watch source, not through timing or real zellij. The watcher itself is tested against real child processes the test spawns.

- **Idle:** no roster, glyph, record or exit change over many batches, and no timeout fires, gives zero snapshots.
- **Pane closed while its session lives on:** the recorded pane process exits while a separate "session" process keeps running. That produces exactly one listing; presence goes Missing; and `lost_labels` reports the label as lost.
- **Agent exits in its pane:** one listing; presence follows the listing.
- **Record watch:** a launch record written for this repo produces one record-change batch; one written for another repo's tag produces none.
- **Rebind:** a record rewritten in place with a new `(pid, birth)` replaces the watch. The old pid's later exit produces nothing, and the new pid's exit produces one listing.
- **A dead retained record is consumed once:** the pane process exits and its record stays on disk. Several later listings, record-dir wakes and re-derivations follow. In total: exactly one exit batch and one exit-triggered listing.
- **A stale queued exit:** an `Exited` for the old key, delivered after the record was rewritten and re-armed, is discarded with no listing.
- **Selection:**
  - a labelled record for another zellij session is not watched;
  - an old record without a label still parses and is not watched;
  - two records for one label in this session are both watched.
- **Registration ordering, through the injected seam:**
  - the birth differs after registration: the registration is removed and the identity is Ended;
  - ESRCH at registration: Ended;
  - inspection failing at either check: Unknown, never Ended.
- **Unknown:** a watch that cannot be established leaves presence as listed, never marks the pane missing, and does not loop.
- **Watcher:**
  - a real spawned child's exit is delivered;
  - a pid already gone, or carrying the wrong birth, is Ended at once;
  - dropping the watch joins its thread, and a test asserts the thread is gone.
- **Glyphs:** changed glyphs give one listing; repeated identical glyphs give none.
- **Unchanged paths:** roster changes, reopens and `probe()` still list and reconcile as before.
- **Mutations, each failing a test:**
  - restoring the timeout probe;
  - listing on identical glyphs;
  - dropping the `Exited` arm;
  - treating Unknown as exited;
  - re-arming a remembered Ended key;
  - skipping the post-registration birth check;
  - acting on an `Exited` whose key is not current.

## Out of scope

- A zellij WASM plugin for layout events.
- Noticing a hand-moved or hand-renamed pane, or a pane started outside `clank agent start`, before the next listing.

## As built

- **`proc_identity::exit_watch`** — `ExitWatch<K>`:
  - One named thread, blocked in kqueue on macOS (`EVFILT_PROC`/`NOTE_EXIT` one-shot per pid, `EVFILT_VNODE`/`NOTE_WRITE` on the directory, `EVFILT_USER` to stop) or in epoll on Linux (a pidfd per pid, inotify on the directory, an eventfd to stop). Elsewhere `new` fails.
  - `arm` returns `Watching`, `Ended` or `Unknown`, through `arm_with` over a `Seam`: identity check, register, identity check again. A birth that changed after registration deregisters and reports `Ended`. ESRCH is `Ended`; an inspection or registration failure is `Unknown`. A verified zombie or dead process is `Ended`.
  - Exits are delivered once, as `Wake::Exited(key)`.
  - Every registration gets its own id. The id rides in the kernel event (kqueue `udata`, epoll data) and is the only thing an event is matched by: a single-map `Ledger` holds the live `(id, key)` per pid. An old registration's event that arrives after a disarm and re-arm on its reused pid finds nothing, so it can neither consume nor tear down the replacement. On Linux, pidfds are keyed by that id too. `disarm(key)` retires only that key's live registration.
  - Drop stops and joins the thread.
- **`pane_target`**:
  - `Launch` gains `label: Option<String>` (`serde(default)`), and `record` writes it.
  - `launches_in(repo, session)` returns this repo's labelled records for one zellij session as `PaneLaunch { record, label, pid, birth }`, through the same safe `read`.
- **The worker** (`status_tui/zellij.rs`):
  - `PRESENCE_PERIOD` is gone, and `worker_loop` blocks on `recv()`.
  - New messages: `Exited(PaneLaunch)`, `Launches(Vec<PaneLaunch>)`, and `Close`. `Close` is sent by `ReconcileWorker::drop`, because the watch holds a sender and the channel alone never disconnects.
  - `watch_launches` builds the watch. It reads the records after the directory is watched, and sends them once.
  - `launch_sink` forwards a directory wake only when this repo's records for this session changed.
  - `Watches` keeps one `(PaneLaunch, Armed)` per record path:
    - `rederive` arms only new or changed records, disarms replaced ones, and never re-arms a remembered one;
    - `exited` counts only the current, still-`Watching` key, once;
    - `absorb` folds both into the batch's `probe`.
- **Listing reasons:**
  - `probe`, including watch traffic;
  - glyphs that differ from `listed_for`, the glyphs of the last answered listing.

  A roster change or reopen that invalidates the pane titles clears `listed_for`, so the next glyphs retitle off a fresh listing. The reconcile and reopen passes keep their own listings as before.
- **`SPINNER_TICK`** is 500 ms.
- **Tests:**
  - `exit_watch`: an old exit dispatched after a re-arm on its reused pid is ignored, and the replacement is delivered once; a displaced registration neither delivers nor tears down its successor; 40 exits racing registration are delivered at most once each, and exactly once when armed `Watching`; the registration ordering table and pid reuse through the seam; a real child's exit delivered once; a gone pid or wrong birth `Ended`; a disarmed exit not delivered; a directory change delivered; drop joining the thread.
  - `pane_target`: selection by repo prefix, session and label; an old unlabelled record parsing.
  - `zellij`:
    - `an_idle_worker_never_lists` waits 5.5 s, longer than the old period;
    - a pane closed while its session lives on: one listing, presence Missing, Lost;
    - a dead retained record consumed once across repeated records, probes and exits;
    - a rebind replacing the watch, with a stale exit discarded;
    - an unknown watch never read as an exit;
    - two records for one label both watched;
    - the sink forwarding only this repo's and session's record changes;
    - identical glyphs not listing, changed glyphs listing.
- **Mutations, each caught:**
  - a restored 5 s timeout probe;
  - listing on identical glyphs;
  - a dropped `Exited` arm;
  - Unknown treated as exited;
  - re-arming an Ended record;
  - skipping the post-registration check;
  - acting on a non-current exit;
  - matching an event by anything but its registration id;
  - losing the displaced registration;
  - dropping the id from the kqueue event.
- **Unrelated, already failing on master:** `git_boundary` reports `stop_hook.rs:4236` and `grok_hook_tests.rs:12`.
- **Linux:** a full cross-check is blocked by `ring`'s C build. The Linux `platform` module type-checks on its own, against `libc`, for `x86_64-unknown-linux-gnu`.
