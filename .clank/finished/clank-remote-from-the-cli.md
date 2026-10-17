# clank-remote-from-the-cli
# `clank remote status|on|off` manages a repo's remote through its owning TUI, with no new socket or port

## Why

Lloyd, via Life's assistent, 2026-10-17 (Signal127): "a CLI API like clank remote to manage the remote interface via CLI". It is lowest priority. Hard gates:
- no new sockets and no new listening ports;
- no new control or admin socket;
- no duplicate ownership, and no weakened authentication;
- credentials never printed into a conversation;
- no restart to adopt it.

Today the remote is owned in memory by the repo's `clank tui` (`status_tui/remote.rs`): the TUI starts the host child, holds the host's admin capability, and is switched only by keys.

## The contract

### Ownership: the existing lease, not a new election

The repo's one remote owner is the TUI holding `status_tui::lease::StatusLease`, the per-repo flock taken for the TUI's life. A second TUI is refused at startup.
- **Holder identity and capability.** `lease::Holder` gains two fields, both `serde(default)`, written with the existing holder file as part of taking the lease:
  - `birth: Option<ProcToken>`. The CLI's view of the owner is that pid and birth, verified natively.
  - `remote_protocol: Option<u32>` (1 for this build). This is the positive evidence of capability. A verified holder without it is an older build; a holder with it is a current one, whatever the status file says.
- **"No owner"** is said only when the CLI can take the flock itself, non-blocking (`LOCK_EX | LOCK_NB`), and releases it at once. That is authoritative absence.
- **"Unknown owner"** is when the flock is held but the holder's identity cannot be read or verified. Unreadable identity is never "no owner".

### Where the files live, and how the owner hears them

All files go in the existing external per-repo runtime namespace (`monitor::runtime_path` / `runtime_dir`), never in `.clank/`:
- `<repo_tag>.remote-status.json`, written by the owner;
- `<repo_tag>.remote-req-<id>.json`, written by the CLI, one per request;
- `<repo_tag>.remote-res-<id>.json`, written by the owner, one per request.

**One runtime watch, owned by the TUI.** The TUI owns one `ExitWatch` with a directory watch on the runtime dir, for its whole life, in or out of zellij. It is created at TUI start, after the lease is taken. Today that watch belongs to the zellij pane reconciler (`watch_launches`), which does not exist outside zellij. It moves up a level:
- The TUI's watch routes `<repo_tag>.remote-req-` entries to the TUI loop.
- It routes this session's `<repo_tag>.pane-launch-` entries to the reconcile worker, when one exists.
- The worker arms and disarms its pane-process exits on the same shared watch (`Arc<ExitWatch>`; its methods take `&self`).

So there is still exactly one watcher per TUI: no duplicate persistent watcher, no polling, and nothing added to `CLANK_WAKE_DIRS`, so no agent or monitor wait is woken by remote control.

**Readiness is advertised, never assumed.** The status file carries `control`:
- `starting`: written as the TUI comes up, before the watch is armed;
- `ready`: written only once the watch is armed and its remote routing is live;
- `unavailable: <reason>`: the native watch could not be built (`ExitWatch::new` failed, or an unsupported platform). The TUI keeps running and the status stays readable; control is refused with that reason.

These files are IPC by file. The promise is **no new socket, port, listener or admin endpoint**.

### Requests: desired state, bound to an owner

**The request.** A request is `{ id, want: on|off, owner: { pid, birth } }`, written atomically: a temp file in the same directory, then `rename`.
- **Wrong owner:** an owner refuses, with result `stale_owner`, any request whose owner identity is not its own. So a delayed writer aiming at a previous owner can never act on its replacement, PID reuse included.
- **One in flight:** the owner handles one request at a time. One arriving while another is in progress gets `busy`. There is no queue. Each request has its own result file, so no caller can overwrite or strand another's.
- **Duplicates:** a repeated notice of the request in flight is ignored, never answered `busy`, so it cannot overwrite that request's eventual result. A request id already answered is ignored, and its result stands.

**Desired-state semantics**, shared with the keyboard. `Remote::toggle` is refactored onto two owner primitives, `start()` and `stop()`; Space calls whichever applies, and a request calls the one it names.
- `on` while On, and `off` while Off: an immediate `applied` result, with nothing restarted or disconnected.
- `on` or `off` while Starting or Stopping: `busy`, with the current state.
- `applied` is written only when the asynchronous transition actually reaches the requested state. A start or stop that fails writes `failed` with its sanitized reason.

**Before writing a request, the CLI reads the status:**
- control `ready`, for the verified current owner: proceed;
- `starting`: wait for `ready` (by its watch, below) up to the deadline, then `unconfirmed: the owner did not become ready`;
- `unavailable`: refuse at once with the owner's reason. This is never described as no owner or an old binary;
- the verified holder has no `remote_protocol`: it is older. Answer at once as below.
- the verified holder has `remote_protocol`, but its status is missing, or names a different owner identity: `not published yet`. A current owner can be between taking the lease and writing its first status, or can have failed that write. The CLI waits on its armed watch for a matching status up to the deadline, then answers `unconfirmed: the owner (pid N) has not published remote-control status`. It never calls this owner older or absent.

No request is written unless control is `ready`.

**The CLI's wait.** The CLI arms its own `ExitWatch` first: a directory watch on the runtime dir, plus the owner's pid for exit. Only then does it publish the request, and immediately after, check whether a result already exists, so an answer that lands between publish and arming cannot be missed. It waits until the deadline (default 30 s, `--timeout`).
- **Owner exits first:** `unconfirmed: the owner exited before answering`. It may or may not have acted, and the CLI says so; it never says either.
- **Deadline:** `unconfirmed: no answer within N s`.
- The CLI removes its own request file on any outcome. The owner removes request and result files it has answered once they are older than 60 s, on its next request.

### Older running TUIs: what the no-restart promise covers

A TUI already running an older build has no handler, and no file can give it one. That cannot change without a restart.
- **What the promise covers:** adopting this feature never restarts anything. Running TUIs are not restarted, not signalled, and their connections are untouched.
- **What it does not cover:** controlling an older owner. An older owner's lease-holder publication has no `remote_protocol`, which is positive evidence from the verified holder, not inferred from a missing status file. The CLI answers at once: `the owner (pid N) runs an older clank without remote control; it will have it after its next restart`. It neither writes a request nor waits.

### The status projection: non-secret by construction

`remote-status.json` is a fixed projection, `{ protocol: 1, owner: { pid, birth }, control: starting|ready|unavailable, control_reason: <sanitized reason or null>, shown: off|starting|on|stopping|failed|unconfigured, url: <public base URL or null>, error: <sanitized reason or null>, updated }`:
- `url` is the base address only. It is never a login link, and never contains a query string or credentials.
- `error` passes through the same `source_diagnostic::sanitize` the source diagnostics use.
- The admin capability, the connector credential, tokens and minted links never enter the projection. Nothing copies `Remote::detail` or configuration wholesale.
- The owner rewrites the file on every remote state change. `clank remote status` prints the projection and the owner.

## Scope

`status`, `on` and `off` through the files above. Excluded:
- minting login links from the CLI (the admin capability is only in the TUI's memory);
- controlling older owners;
- any socket, port, listener or admin endpoint.

## Acceptance

Tests drive the real `StatusLease`, the reconcile worker's directory-watch sink, and the async `Remote` state machine through their injected boundaries (fake host start and stop), not only serialization round trips.
- **Ownership:**
  - with the lease held, the CLI never reports "no owner";
  - with it free, the CLI does;
  - an unreadable holder identity reads "unknown".
- **Desired state:**
  - `on` while On and `off` while Off are no-op `applied`, with no start or stop call;
  - `on` from Off is `applied` only after the fake host reaches On;
  - a failing start gives `failed`, with seeded secrets absent;
  - `on` or `off` during Starting or Stopping is `busy`;
  - Space still toggles.
- **Races:**
  - a request bound to a previous owner identity is refused `stale_owner`, PID reuse included;
  - two competing writers: one is `applied` and the other `busy`, both answered;
  - a repeated id is not re-applied.
- **Waiting:**
  - the owner exits before answering: `unconfirmed`;
  - the deadline passes: `unconfirmed`;
  - an older owner (a verified holder with no `remote_protocol`): answered at once, with no request written.
  - **Startup interleaving:** a current owner is held after taking the lease and publishing its holder, but before its first status write. The CLI arms, re-reads and waits. It reports neither older nor absent, and writes no request. When `ready` appears, it proceeds; at the deadline, it answers `unconfirmed … has not published`.
  - **A failed status write:** a current holder whose status file never appears gets the same not-published outcome. It is never called older or absent.
- **Projection:** with a configured URL, a failing start and seeded secrets (a token, a credential, a `?t=` link), neither the status file nor the CLI's output contains any of them.
- **No wake churn:** a request file causes no event in `repo_watch` or the agents' waits.
- **Every owner context is served or refused truthfully:**
  - a TUI outside zellij (no reconcile worker) still receives a request through its own watch and applies it;
  - with the zellij session unidentified, remote control still works, and only the pane-launch routing is absent;
  - when the native watch cannot be built, the status says `unavailable`, and the CLI refuses at once with that reason, writes no request, and never reports no owner or an older binary.
- **Readiness:** a status of `starting` makes the CLI wait for `ready`, and a request written only after `ready` is delivered.
- **The wait:** a result that lands before the CLI's post-publish check is still seen, and a repeated notice of the in-flight id does not overwrite its result with `busy`.

## As built

- **`cli/remote_control.rs` (new):** the protocol.
  - Types:
    - `Owner { pid, birth }`, `Control`, `Shown`;
    - `Status`, the fixed projection;
    - `Want`, `Request { id, want, owner }`, `Outcome` (`applied`, `busy`, `failed`, `stale_owner`), `Response`.
  - Files: `status_path`, `request_path` and `result_path`, under `monitor::runtime_path` (`<runtime>/<repo_tag>.remote-*.json`, never in the repo); `request_id` recognizes only this repo's request files.
  - `write_atomic` (temp file then rename) and `read`.
  - The projection: `safe_url` strips user info, query and fragment. `safe_reason` masks the known secrets it is given, cuts every URL in the text to scheme, host and port (an unparseable one becomes `[address]`), then applies `source_diagnostic::sanitize`. A short login token under `?t=` is therefore removed by structure, not by a shape heuristic.
  - `decide` (owner side): an answered id, or the in-flight id seen again, is `Ignore`. A wrong owner identity (PID reuse included) is `stale_owner`. Another request in flight, or Starting/Stopping, is `busy`. Asking for the current state is `applied` at once. Otherwise `Start` or `Stop`.
  - `completion` answers only when the asked state is reached, or `failed`.
  - `readiness` (CLI side) gives `NoOwner` only when the CLI took the flock itself. An unreadable or unverifiable holder is `Unknown`. A verified holder without `remote_protocol` is `Older`. A holder, or its matching status, speaking a protocol other than `PROTOCOL` is `Unsupported`. A current holder with a missing, foreign or `starting` status is `Wait(owner)`. Then `Unavailable { owner, reason }` or `Go(owner)`. Every current-owner verdict carries the `Owner` it is about.
  - `set` (the CLI's `on`/`off`) settles at once when there is no current owner to ask; otherwise `set_observed` runs over an `Observe` (production: `Watched`, one `ExitWatch<Owner>` on the runtime directory):
    1. reads readiness, which reads the status once;
    2. if the owner it names is not the one armed, arms that owner's exit, handling `Ended` and `Unknown` as `unconfirmed`, and reads again before concluding anything, so an owner replaced before the request is armed afresh;
    3. waits for `ready`; only the armed owner's exit counts, a retired owner's is ignored;
    4. writes the request, addressed to the armed owner;
    5. waits for the result, that owner's exit (`unconfirmed: exited before answering`, unless its answer landed first) or the deadline;
    6. removes its request file.
  - Both waits check the absolute deadline on every turn, before reading, and take wakes in batches of at most 1024, so a directory that never goes quiet cannot keep `set` past `--timeout`.
  - `run` implements `clank remote status|on|off [--repo] [--timeout 30]`. `status_text` prints the owner and readiness, and the projection only when the status names that current owner.
- **`status_tui/lease.rs`:**
  - `Holder` gains `birth` and `remote_protocol` (`serde(default)`), written by `acquire` with the holder record inside the lock.
  - `probe(repo)` takes the flock non-blocking and releases it at once (free means authoritative absence), or reads the holder.
  - Test-only `hold_as` names another process as the holder.
- **`status_tui/remote.rs`:** `toggle` now dispatches to two desired-state primitives, `switch_on` (from Off or Failed only) and `switch_off` (from On only), each a no-op otherwise. They use the same start and stop tasks as before. New accessors `base_url`, `failure` and `secrets`. The remote keeps every connector and administration credential an instance has held (`BridgeInstance::known_secrets`), so a later failure quoting one is masked. A first attachment the connector refuses is masked inside `BridgeInstance::start_ready`, the one place that attempt's credentials are known, before the instance is discarded.
- **`status_tui/zellij.rs`:** `RuntimeWatch` replaces `watch_launches`.
  - The TUI owns it from startup, in or out of zellij.
  - Its one `ExitWatch` on the runtime directory sends this repo's new request ids (`RequestScan`) to the loop and wakes it.
  - It routes launch records and pane exits to the reconcile worker once the worker attaches (`attach_worker`), and the worker arms its pane exits on the shared `Arc<ExitWatch>`.
  - `unavailable` holds why there is no watch.
  - `LaunchFeed` replaces `launch_sink`: reading the records, comparing and sending happen under one lock, which `attach` takes too. Before a worker attaches a directory change reads nothing, and attaching reads afresh, so the worker never hears an older snapshot after a newer one.
- **`status_tui/remote_owner.rs` (new):** `Served`.
  - `starting` publishes `control: starting` before the watch exists. `armed` publishes `ready`, or `unavailable` with the reason, including when its own identity is unreadable.
  - `publish` rewrites the status only when the projection changes.
  - `request` handles a request through `decide`, starting or stopping the real remote via the `Switch` trait (implemented for `Remote`). Every published reason is masked with the switch's known secrets.
  - `progress` answers the request in flight on completion.
  - `respond` writes the result atomically.
  - `sweep` removes this repo's request and result files older than 60 s.
  - `pump` is one turn of the remote for the loop: requests heard, the remote's finished tasks drained, the in-flight request answered and the status published. It returns the notices for the loop to show.
- **The TUI loop (`status_tui/mod.rs`):** after the lease, `Served::starting`, then `RuntimeWatch::start`. The reconcile worker spawns on that watch. After the remote is built, `served.armed`. Each pass calls `remote_owner::pump` before the document-overlay branch, so a request accepted while an overlay is open completes and is answered while it stays open; the latest notice is kept and shown once no overlay is up.
- **Tests:**
  - `remote_control`: decisions, completion, readiness from evidence (absent, unknown, unverified, older, waiting, foreign status, ready, unavailable), the projection never carrying a credential, and request-file recognition.
  - `status_tui::lease`: the probe tells a free lease from a held one, with the holder's identity and protocol.
  - `status_tui::remote_owner`, through the real lease, the real `RuntimeWatch` and `Served`, a fake `Switch`, and the real CLI `set`:
    - a request reaches its TUI, with no zellij worker present, and is applied; asking again changes nothing; the status shows a base address with no secret;
    - `applied` waits for the transition, and a second caller is `busy`;
    - a failed start is reported without its secrets: a `?t=` login link, URL user info and a known short connector credential, keeping what failed and the host;
    - a failed remote's published status and `clank remote status` carry the same reason, equally masked;
    - the deadline holds in both waits while the directory never goes quiet (an `Observe` that wakes forever);
    - an owner replaced after the first is armed and before the request: the request goes to the replacement, and the retired owner's exit does not end the wait (the real exit watch, with a hook at arming);
    - an owner speaking another protocol is refused, and no request is written;
    - a status no current owner published is not shown, with no owner and with a different one;
    - an unavailable owner is refused at once with its reason, and no request is written;
    - a current owner that has not published is waited for, then `unconfirmed`, never called older, and no request is written;
    - an owner that dies before answering is `unconfirmed`;
    - a request seen twice is applied once.
  - `status_tui::remote`: on while on, and off while off, do nothing, with no new start generation. Through `pump` alone, with a real remote, a real start and a real stop are each answered `applied` with every notice left unshown, and the remote keeps its instance's credentials. A first start whose real connector handshake is refused with the credential it was sent answers `failed` with the refusal's wording, and neither the result, the published status nor the operator's notice carries the credential.
  - `status_tui::zellij`: the feed forwards only this repo's record changes; a change read while the worker attaches never arrives after the attach's newer snapshot.
- **Not covered by a test:** the one `pump` call's place before the overlay branch in `status_tui/mod.rs`, since `run_tui` owns the terminal and is not driven in-process; `pump` holds everything a request needs, so that placement is the whole of the wiring. The reconcile worker's arming of pane exits on the shared watch inside zellij is exercised only through `Arm`.
- **Mutations, each caught:**
  - arming only the first owner seen, not a replacement;
  - dropping the deadline check in either wait;
  - accepting another protocol from the holder, or from the status;
  - printing a status that names no current owner;
  - publishing reasons without cutting URLs;
  - masking a failed start, or the published status, without the switch's secrets;
  - `pump` not answering the request in flight;
  - the remote not keeping its instance's credentials;
  - the old `launch_sink` order: reading the records before taking the lock;
  - not masking a refused first attachment with its credentials;
  - the CLI not arming the owner's exit;
  - publishing the raw URL;
  - reading a missing status as an older owner;
  - the runtime watch not routing request ids.
