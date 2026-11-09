# zellij-session-found-live
# clank finds a process's zellij session from the process itself, never from a name it was told

## Why

LLFourn's TUI for the backup-bip-experiment workspace showed `! zellij` again after every restart.
Every pane there carries `ZELLIJ_SESSION_NAME=clank-backup-bip-ex-fa74`. clank addresses zellij by
that name (`open_zellij.rs:35`, `:1370`, `:2085`, `:2597`; `status_tui/lease.rs:42`), and zellij
answered "Session 'clank-backup-bip-ex-fa74' not found". So the TUI's pane listings failed, which
set the badge (`status_tui/zellij.rs`, `Observed::note`). Its tab retitle failed, and the tab still
reads `clank 0.1.0+e9a45613`. Recovery nudges and permission-block `--nudge` into that workspace
fail the same way.

The cause, from the installed zellij's source (`~/.cargo/bin/zellij`, built from upstream
`87276681`, 3 Nov 2026):

- A session's socket is named by a random 12-hex **ID** (`generate_session_id`,
  `zellij-utils/src/sessions.rs`).
- A SQLite index (`…/contract_version_1/session_index.sqlite`) maps names to IDs. `--session <x>`
  looks `x` up in the index, then falls back to treating `x` as an ID.
- A rename edits only the index row.

The row for `clank-backup-bip-ex-fa74` disappeared from the index; zellij deletes rows by name in
several paths, and which one fired here isn't proven. The live session survived as its bare ID
`cd24109dee91`. LLFourn's rename to `right` then inserted a new row. Panes still hold the old
name in their environment, which zellij sets once, at spawn.

The deeper fault is clank's: it **pins** a session by name, both in the environment it trusts and
in `clank open`'s deterministic `clank-<basename>` lookup. Any rename, a lost index row, or a
rebuilt workspace breaks that.

A process can't change sessions. zellij has no way to move a live pane between sessions, and a
pane's processes are descendants of the server that spawned its pty. So the session a process
belongs to is a fact about the process, discoverable at any time.

## Build

### 1. One resolver: the server endpoint a process belongs to

`fn endpoint_of(pid) -> Result<Endpoint, Unresolved>`, inside the existing zellij layer:

- Walk the parent chain from `pid` to the first `zellij --server <socket path>` process. Read
  process ancestry and command lines through `proc_run` or the existing process-inspection helpers;
  no new `unsafe`.
- An **`Endpoint`** is the full socket path, the server's executable, its PID and its start time.
  It is never a bare socket filename. Two roots can each hold a session with the same legacy name,
  so the filename alone identifies nothing.
- **Three outcomes**, kept apart:
  - `Found(Endpoint)`.
  - `NotInZellij`: the whole ancestry was read, and no server is in it.
  - `Unknown(why)`: process inspection was denied, raced or failed. This is never treated as
    `NotInZellij`.
- A long-lived process (the TUI) may cache `Found`, since a process can't change sessions. It never
  caches `Unknown`.

### 2. Addressing an endpoint, and keeping pane authority where it is

**Invocation.** A zellij client resolves `--session <x>` inside its own socket directory:
`$ZELLIJ_SOCKET_DIR`, else its runtime or temp dir, plus its contract directory
(`zellij-utils/src/consts.rs:319` in the installed source). So every clank-issued `zellij`
call for an endpoint:

- runs the endpoint's own server executable, or one proven to use the same contract directory;
- sets `ZELLIJ_SOCKET_DIR` to the root that makes the client's socket directory equal the
  endpoint's;
- passes `--session <current name>`, where the name is found from the endpoint's ID (see below);
- refuses if the directory the client would use can't be made equal to the endpoint's.

**Finding the name (LLFourn: "it can just find the name").** zellij refuses `--session <id>`
once a session has a name. Its action path checks the requested name against `get_sessions()`
(`src/commands.rs` `send_action_to_session`, ~529), which lists each socket under its index name.
It also has no flag that addresses a session by ID or socket path. So each call looks the
endpoint's ID up in zellij's own index:

- The index is `session_index.sqlite` under zellij's session-info cache dir (`default_index_path`,
  `zellij-utils/src/session_index.rs:148`). It's shared across socket roots.
- Read it read-only: `SELECT name FROM sessions WHERE COALESCE(id, name) = ?`. That is zellij's
  own `display_name_for_session_id` rule.
- No row means the ID is the name, which is zellij's own fallback.
- If the call then fails with zellij's "not found" (a rename landed in between), look the name up
  once more and retry once. A second failure is reported, never retried in a loop.
- A locked, unreadable or unexpectedly shaped index is `Unknown`. It is never guessed, and never
  treated as "no row". An index file that doesn't exist is "no row": without one, zellij names
  sockets by session name and lists them by filename. That's released 0.46.
- Legacy sockets (named by their name, no ID) resolve to their own filename.

**Accepted limitation: a name reused mid-call.** A call is bound to a name, not to the socket.
zellij offers no way to address a session by ID or socket path.

- **The race:** between the lookup and the call, session A is renamed and another session B takes
  A's old name. The call then reaches B, and nothing fails to say so. A write or interrupt for A's
  pane could land in B's pane with the same ID.
- **Why it's accepted:** it needs a person to rename one session and reuse its exact old name for
  another within the milliseconds between two subprocess calls. LLFourn judged that window too
  small to design around: "Zellij renames are not going to happen mid-call ... this is a very small
  window."
- **The rejected alternative:** a private `ZELLIJ_SOCKET_DIR` holding a hard link to the resolved
  socket would bind each call to the server. It was rejected for its on-disk lifecycle and
  path-length constraints against a window this small.
- **What clank still guarantees:**
  - The lookup happens immediately before each mutating call, never from a cached name.
  - The plan doesn't claim endpoint identity for a successful name-based call.
  - A plain rename, with no reuse of the old name, is caught by the not-found retry.

It never relies on an inherited `ZELLIJ_SESSION_NAME`, and never on the name being in the index.

**Which endpoint.**
- The TUI uses its own: `endpoint_of(self)`.
- Every other caller that targets a repo's workspace (nudges, recovery, permission-block `--nudge`,
  `lease.rs`, in-session `clank open`) uses the endpoint whose panes run that repo's
  `clank tui --repo <repo>`. That's the same scan as §3, so the workspace stays anchored on the
  repo's TUI.

**Pane authority is unchanged.** Inside the chosen endpoint, `pane_target`'s existing rules still
pick the agent's pane:

- the fresh, live pane named for the agent in the TUI-anchored workspace;
- refusal on duplicates, exited panes and ambiguity.

A harness process's ancestry or its inherited `ZELLIJ_PANE_ID` never selects a pane to type into.
Those can be stale, the agent may have no binding yet, and a background Claude's process doesn't
share ancestry with the `claude attach` frontend showing it (`agent.rs:513-515`). This plan changes
only how the session is reached, not which pane in it is the agent.

### 3. `clank open` finds the repo's session by what is running in it

From outside a pane, `clank open <repo>`:

- Lists live sessions by socket, not by name.
- Looks for the one whose panes run this repo's `clank tui --repo <repo>`.
- One match: use it. None, on a complete scan: create a session. Two or more: refuse, naming each
  endpoint and display name.
- A scan that couldn't read every live endpoint (unreadable directory, an endpoint that doesn't
  answer) is unknown, not zero matches. It refuses as `open_zellij::require_complete` does today,
  and never creates another session.
- Nothing is recorded about which session a repo lives in.
- The deterministic `clank-<basename>` name, and its `SESSION_NAME_MAX` cap, remain only as the
  display name for a new session. Check whether the cap is still needed for zellij versions before
  IDs, and model those separately from ID-based versions; don't let either version's rule leak into
  the other's path.
- Remove the name-based dedup and delete-dead-then-create paths if the scan replaces them. Say what
  the dead-session case now does.

### 4. Say what is wrong

The TUI badge distinguishes:

- **not in a zellij pane:** no server above the TUI;
- **session not answering:** the server was found, but its socket doesn't answer;
- **couldn't tell:** process inspection failed. The badge says so, with the reason.

A healthy session whose socket is an ID, or whose name changed, is just connected. It is never
treated as a rename or an error.

## Out of scope

- Renaming or repairing a user's live session.
- Asking zellij upstream for `ZELLIJ_SESSION_ID` in pane environments, or reporting the lost index
  row. LLFourn files those, if at all.

## Tests

No test starts zellij or a server. Process tables and listings are faked at the layer's boundary.

1. **The resolver.**
   - A process two parents below `zellij --server <dir>/cd24109dee91` resolves to that full path,
     whatever `ZELLIJ_SESSION_NAME` says.
   - The older form `--server <dir>/clank-life` resolves to its full path.
   - A complete ancestry with no server gives `NotInZellij`.
   - Denied or raced inspection gives `Unknown`, never `NotInZellij`.
   - A `zellij` client process (not `--server`) is not taken for the server.
2. **Invocation.**
   - A caller whose own socket-root environment is stale or different still reaches the endpoint,
     because the issued argv and environment point the client at the endpoint's directory.
   - Two roots each holding a legacy session of the same name are told apart.
   - A client that can't be pointed at the endpoint's directory is refused.
   - The name is found from the index: an ID with a row passes its current name, and one with no
     row passes the ID.
   - A rename between the lookup and the call is retried once with the new name, and fails after
     that.
   - A mutating call looks the name up immediately before dispatch: the lookup isn't served from a
     cache.
   - A locked, unreadable or reshaped index gives `Unknown`; an absent index file is "no row".
   - A test pins the index columns clank reads against zellij's `CREATE TABLE`, so a schema change
     fails loudly.
   - A source-scan gate, if cheap, checks that no addressing reads `ZELLIJ_SESSION_NAME`.
3. **Pane authority.** No case below may type into the wrong pane:
   - an unbound or newly opened agent;
   - a background Claude shown through `claude attach` in a pane;
   - a stale binding, or an inherited `ZELLIJ_PANE_ID` that disagrees with the live pane named for
     the agent;
   - duplicate and exited panes.

   Each resolves through `pane_target`'s existing rules, or refuses.
4. **`clank open`.** Each case uses faked listings:
   - one endpoint running the repo's TUI is reused;
   - none, on a complete scan, means create;
   - two means refusal naming both;
   - a renamed or nameless session is still found;
   - an incomplete scan (unreadable or unanswering endpoint) refuses and creates nothing.
5. **Badge.** Not-in-zellij and not-answering read differently. A connected ID-named session shows
   no badge.
6. **Version pin.** A test fails if the zellij this builds against stops naming its socket on the
   server command line. For example, check the expected `--server` argv shape in the layer's
   recorded spawn, or document the probe a person runs.

## Done when

- **The original workspace:** in a session with a stale `ZELLIJ_SESSION_NAME`, renamed, or missing
  from the index, the TUI, nudges and `clank open` all work without restarting anything.
- **No name addressing:** no clank code path addresses zellij by an environment or deterministic
  name.

## As landed

- **The resolver.** `zellij_endpoint.rs` walks process ancestry to the server and returns
  `Found(Endpoint)`, `NotInZellij` or `Unknown`. Process reads are native, with no new `unsafe`.
  `own_endpoint()` caches only `Found`.
- **Invocation.** Each call runs the server's own executable with `ZELLIJ_SOCKET_DIR` set to the
  socket root, with `ZELLIJ_SESSION_NAME` removed, and with `--session <name>`.
  - The name is read from `session_index.sqlite` immediately before each call, with one retry on
    not-found.
  - An absent index file means the socket's filename is the name: released 0.46 (Homebrew) writes
    no index. A locked, unreadable or reshaped index is Unknown.
- **The client's identity.** A client is used only if it's the running server's image:
  - Linux compares `/proc/<pid>/exe` with the client by device and inode.
  - macOS uses the first `txt` record from `/usr/sbin/lsof`. It fails closed, with no timestamp
    fallback.

  An upgraded binary under a live older server therefore refuses until that server restarts.
- **Accepted limitation.** A name reused between the lookup and the call (LLFourn's call) is
  documented in the module, together with the rejected hard-link binding.
- **`clank open`.** It scans servers by process, requires a complete scan, and finds the repo's
  workspace by its TUI.
  - Removed: `decide_session`, `DeleteDeadThenCreate`, `delete_argv` and `plan_all`.
  - An exited session holding the conventional name counts as taken, so the new session gets the
    next free name.
  - `SESSION_NAME_MAX` is kept for every new session's display name, because the era of the PATH
    zellij can't be read without starting it.
- **Pane authority.** It's unchanged: `pane_target` still picks the pane, and ancestry or
  `ZELLIJ_PANE_ID` never does.
- **Badge.** `! no zellij`, `! zellij not answering` or `! zellij unknown`. The full unknown reason
  goes to a log notice.
- **Dependency.** `rusqlite` 0.31 (bundled, MIT) is added, with notices regenerated.
- **Known flaky (recorded).** `only_a_reopen_request_in_the_runtime_directory_reaches_the_loop`
  fails on unchanged master under load. It was unrelated, and passed in the full integration and
  library runs here.
