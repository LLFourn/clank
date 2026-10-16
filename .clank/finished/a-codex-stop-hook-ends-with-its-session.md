# a-codex-stop-hook-ends-with-its-session
# A parked codex Stop hook, and the wait it owns, end when the codex session it serves ends

## Why

Observed 2026-10-16, after every zellij session was killed: 96 `clank stop-hook --tool codex` processes and their `clank wait --die-with-owner` children were still alive, for codex sessions that no longer existed.
- 29 hooks were orphaned to launchd (ppid 1), some for 11 days, under standalone `codex --no-daemon` sessions that had exited.
- 20 hooks were children of the codex app-server daemon (`codex app-server`). The daemon serves many sessions and outlives each of them.

The wait's lifetime is bounded: `--die-with-owner` ends it when the hook closes its stdin pipe (`owner_sentinel`). The hook's own lifetime is bounded by nothing. A parked hook blocks until work arrives for its author, so for an agent that is gone it can block forever, holding a wait, a repo watcher and a monitor connection.

Claude hooks did not leak the same way in this sample; check why before generalising.

## What to build

### The owner, resolved once at hook start

The hook resolves one owner: a verified `(pid, birth)` whose exit ends this hook. It is resolved once, before the wait is spawned. Each route below says which evidence counts. Anything else is **Unknown**: no owner watch, and today's behaviour, which still ends the wait on a rebind because the wait already refuses a binding it no longer owns (`WaitBinding::ensure_current`, `WaitSuperseded`).

**What the owner means.** The owner is the interactive codex client that ran this turn. Its exit bounds the hook to that client. It does not claim the durable codex thread is gone: a thread resumed later gets a new turn and a new hook.

**Standalone route.** The hook's parent is a codex client.
1. Read `getppid()`.
   - If it is 1, the spawning process has already exited and we were reparented: the owner is **already gone**. Exit silently before spawning anything.
2. Read the parent's birth token, then read `getppid()` again.
   - If it changed, the parent died during the read, and the token may belong to a stranger: already gone, exit silently.
   - If it is unchanged, the token was taken while that process was still our parent, so it is our parent's.
3. Inspect the verified parent's argv. The executable must be `codex`, and it must not be running `app-server`. Otherwise this is not the standalone route.

**App-server route: deferred, always Unknown.** A hook whose parent is the codex app-server daemon gets no owner watch. Nothing available to the hook relates a client process to this hook's exact thread:
- The hook's environment is the daemon's, inherited from the pane the daemon was first started in (measured 2026-10-16: `ZELLIJ_PANE_ID=0` while the client ran in pane 1).
- The binding proves label to thread, and a launch record proves label to a verified client process, but nothing links that process to that thread.
- `agent.rs` publishes a replacement client's record before that client binds. During that window, an existing hook for thread A would see A's still-valid binding and a uniquely qualifying record for a process that has never owned A.

Inferring the link from the label would be killing on a guess. A later plan needs a client-specific lifecycle or daemon signal; one candidate is launching clank's codex agents with `--no-daemon`, which makes the client the hook's parent and brings them onto the standalone route.

**Any other parent** (a shell, a test harness, a subreaper) is Unknown.

### The owner watch and its priority

With an owner resolved, the hook arms `proc_identity::exit_watch` on it. Registration is bracketed by identity checks, so an owner that ended in between reads as Ended, and the hook exits silently.

When the owner exits, that wins over anything the wait delivers. In the hook's single wait-for-result point, owner loss is checked first:
- it drops the owner pipe, so the wait's existing `--die-with-owner` sentinel ends it;
- it waits for the wait to exit;
- it presents nothing, writes no presentation receipt, and acknowledges nothing;
- it exits.

A wake already received but not yet presented when the owner ends is discarded. The watch thread is joined on every exit path.

The existing owner pipe between hook and wait is unchanged. Claude hooks are unchanged: their parent is the claude client itself, and none leaked in the sample.

## Acceptance

Tests use injected identity and notification sources, and test-owned processes and pipes. They never spawn the clank binary or a real codex.

- **Standalone:**
  - parent already 1 at start: exits silently, spawns no wait;
  - parent changes between the two `getppid` reads: exits silently;
  - a verified codex parent that exits after the watch is armed: the hook ends and its wait ends.
- **App-server is Unknown:** a daemon parent with a binding naming this session and exactly one live, verified codex launch record for the label still gets no owner watch, and the hook behaves as today. This includes a replacement client's record published before it rebinds.
- **Any other parent** (a shell or a test harness) is Unknown.
- **Owner loss beats a queued wake:** a wake ready at the moment the owner ends produces no presentation, no receipt and no acknowledgement; the wait is gone and the watch thread joined.
- **Mutations, each failing a test:**
  - treating ppid 1 at start as Unknown;
  - dropping the second `getppid` read;
  - giving a daemon parent the standalone route;
  - presenting a queued wake after owner loss.

## As built

- **`cli::hook_owner`** (new):
  - `resolve(&impl Parentage) -> Owner` returns `Process { pid, birth }`, `Gone` or `Unknown`.
    - A parent of 1 is `Gone`.
    - The parent's birth token is read between two `getppid` calls; a changed parent is `Gone`.
    - The verified parent's argv must be `codex` without `app-server`. `Parentage::argv` returns a typed `Argv`: `Read`, `Ended` (the captured birth has exited, been replaced, or is a zombie) or `Unreadable`. `Ended` keeps the death evidence: it is `Gone`, not `Unknown`.
    - Anything else, including an unreadable token or argv, is `Unknown`.
  - `Lifetime::of(owner)` arms `proc_identity::exit_watch` on the owner. The result is `Bound` while it is watched, `Gone` when the owner ended before the watch was armed, and `Unbound` for `Unknown` or when no watch exists. `gone_now()` and `ended()` read it; an `Unbound` lifetime never ends.
  - `lifetime_for(tool, src)`: only `Tool::Codex` resolves an owner; every other tool is `Unbound`.
- **The Stop hook** (`stop_hook.rs`):
  - `compute_wait_outcome` takes the lifetime once, before arming.
  - `await_wait_under_deadline` takes `&mut Lifetime`:
    - a gone owner returns before `arm`, so no wait is spawned;
    - the wait runs through the `Run` trait's `run_until(life.ended())`, which returns `Ran::Finished(output)` or `Ran::Abandoned(shutdown)`. For the real `OwnedWait`, owner loss closes the owner pipe and hands back the child's collection as the shutdown, so the wait's own EOF shutdown (`source_group::exit`, which retires its producer groups) runs before the hook returns;
    - the poll deadline bounds only the waiting. A `Ran::Abandoned` shutdown is awaited outside it, under `ABANDON_GRACE` (10 s), well inside the runner's 300 s margin. A wait that outlives the grace is killed, on drop. A deadline that fires with the owner already gone is `OwnerGone`, not a re-arm;
    - a final `gone_now()` after the wait finishes discards anything it found.
  - Each of those returns `Silent { OwnerGone }`, so nothing is presented and no receipt is captured.
- **`SilentReason::OwnerGone`** (core) maps to `NotOurs::OwnerGone`, and is classified as an intentional silence.
- **Tests:**
  - `hook_owner`: ppid 1 is `Gone`; a parent that changes between reads is `Gone`; a verified codex parent is the owner; a daemon, a shell, missing argv or an unreadable token is `Unknown`; only codex takes an owner; a real child's exit ends a `Bound` lifetime; an owner already exited is `Gone`; `Unknown` never ends.
  - `hook_owner`: an owner dying during its argv read (a real child killed at that boundary, read through `Native`) is `Gone`.
  - `stop_hook` (paused clock): owner loss just after arming, with a 1 s poll deadline and a 2 s shutdown, returns `OwnerGone` with the shutdown completed.
  - `stop_hook`: owner loss closes the pipe and collects a real child after its EOF shutdown writes its marker; an owner gone before arming spawns no wait; an owner ending mid-wait drops the wait (a drop guard proves it) silently; owner loss outranks a wake the wait already found; a live owner lets the wake through.
- **Mutations, each caught:**
  - ppid 1 read as `Unknown`;
  - the second `getppid` read dropped;
  - a daemon parent given the standalone route;
  - presenting a wake found after owner loss;
  - `Tool::Codex` left `Unbound`;
  - no `ended()` arm in the `select!` (caught once the mid-wait test bounds its time well inside the 30 s deadline);
  - no pre-arm check;
  - `Argv::Ended` read as `Unknown`, or `Native` mapping an ended inspection to `Unreadable`;
  - an abandoned wait not collected (killed on drop);
  - the abandoned shutdown not awaited;
  - no owner-gone check on the finished or expired paths.
