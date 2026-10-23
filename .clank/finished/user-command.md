# user-command
# A person's press types `! <command>` into the asking agent's terminal, and nothing else can

## Rules

- Commit in your repo. Do not push or post anywhere.
- Commit trailers follow the clank master's standing rules.
- Comments explain WHY, never WHAT. Delete any comment the code already says; keep only rationale,
  constraints, hazards and rejected alternatives. Public items get a short doc comment giving the
  contract.
- Test against throwaway repos and panes. Never type a real outward command (push, PR) while
  testing.
- No new socket, listening port or IPC/admin control socket (LLFourn's standing constraint on
  clank).
- Report to `clank send --repo ~/src/clank-wr --to claude -F <file>`.

## Why

Agents regularly reach actions their permission checks deny but LLFourn is happy to do himself:
pushing a branch, opening a PR. Today an agent pastes a `! …` line in chat and he types it. The story
site (clankstack, a separate tool) will show such a command with a Run button. LLFourn's design:
pressing it doesn't execute anything in the site. clank types `! <command>` into the terminal of the
agent that asked. Claude Code's bash mode runs it as the user's own input, and its output lands in
that agent's conversation. No result has to be carried back.

This plan is clank's half: a way to deliver a user command into an agent's terminal. The site's
half (the display, the press, the record, the signing key) is planned in clankstack once your
report gives the interface.

## The hazard, and what must hold

Bash mode runs without the agent's permission checks. That is the point, and it is the danger. **A
`!` line may reach an agent's prompt only because a person chose it.**

- **No ordinary path can produce it.** Every ordinary write (nudge, send, event wake, recovery,
  check-in, the web/remote chat) passes one admission rule at the common write boundary that makes
  shell mode unreachable from its text. A terminal write proceeds only when the pane's input is
  positively read as not in shell mode, or when the write opens with a key proven to leave it.
  Where neither holds, the write is refused and event delivery stays pending.
- **The user-command path needs proof no agent is given.** The site and every agent run as the same
  Unix user, so this cannot be an OS boundary: an agent could run `zellij action write-chars`
  itself. The boundary is that no agent holds the site's signing key, that agents' deny rules name
  it, and that a captured request authorises only the one command it was signed for.
- **The target is fixed by the request.** Repo, label, bound session and tool are signed. Clank
  never guesses a pane and never sends a queued command to a successor under the same label.
- **Only at an idle, empty Claude prompt**, read from one pane dump, triggered by existing wakes.
- **At most once.** A request is dispatched at most one time and never replayed; an outcome clank
  cannot know is reported as unconfirmed, not retried.
- **Recorded.** Every delivery, refusal and failure goes in the agent's nudge history.

## What the code gives us (verified)

- Ordinary writes all funnel into `crates/cli/src/cli/pane_input.rs`: `say_argvs` (:16) builds a
  `write-chars -- <text>` then a separate `write … 13` (Enter); `submit_with` (:204) maps a failure
  of the first write that never started to `PaneWrite::NotStarted` and every later failure to
  `Unknown`. `interrupt_and_submit_async` (:138), `wake_for_events_async` (:240), `submit_async`
  (:190), `say_to_pane_for_tool` (:310, sync, via `say_to_pane_with_delay` :336) and `stop_pane`
  (:66) are the entry points. LF inside text is already the multi-line mechanism (doc at :6-10);
  Tab is a Codex submit/queue key (`codex_queue_argv` :44).
- `open_zellij::PaneWrite` (`open_zellij.rs:2004`) is `NotStarted` (transport never ran) or
  `Unknown` (ran, may have written); `run_pane_write` bounds each write at 3 s. The raw transports
  `write_pane_action` (:1955), `write_pane_action_async` (:1992) and
  `write_pane_action_classified` (:2020) are `pub(crate)`, so any module can reach around
  pane_input today.
- Callers that write to panes: `nudge.rs:65` (interrupt then submit), `event_interrupt.rs:223`
  (event wake; tries `codex_interrupt::try_wake` first, :211), `monitor.rs:4287` (check-in
  `Typist::submit`), `monitor.rs:4517` (recovery `Transport::Typed`), `web/mod.rs:139-182` (`say`,
  trying `codex_chat::send` first, and `stop`). `status_tui` has no direct writer; it goes through
  these. No other `write-chars`/`write`/`send-keys` exists in production code (grepped `crates/`);
  the only `zellij` spawns are in `open_zellij.rs`, enforced by
  `tests/it/zellij_ownership_boundary.rs`. Local bug found: `web/mod.rs:168` types into any
  non-Codex pane as `Tool::Claude`, while `stop` (:175) resolves the pane's real tool.
- Native, non-terminal text paths: `codex_chat::send` (`codex_chat.rs:133`),
  `codex_interrupt::try_wake` (`codex_interrupt.rs:15`), and OpenCode's plugin resume
  (`resume.rs` `Resume::Plugin`).
- Readiness evidence already exists: `composer::composer` (`composer.rs:64`) reads the input line
  from an `--ansi` dump (`open_zellij::dump_pane_screen`, :1972) as `Empty`/`Draft`/`Unreadable`;
  Claude's input is the last `❯` line bounded by `───` rules, its placeholder the dim `Try "…"`
  (:21-35). Recovery and check-ins already refuse anything but `Empty` (`monitor.rs:4140`,
  `:4355`) inside the loop's last look (`may_send_now` :4125, asked through `authorized` :4079).
  Grok and OpenCode have no prompt glyph, so they always read `Unreadable`.
- Idle is two facts the monitor owns: recovery's record read (`recovery.rs:2498` `activity`: the
  anchored latest turn is `Completed`) and turn custody (`turn_custody.rs`, persisted in
  `.clank/agents/<label>/turn.json` :285; `SessionState.outstanding`). `state.json`
  (`agent_state.rs:105`) is a published projection and decides nothing. **A parked agent is not
  idle:** the core answers a turn-end with nothing owed by `Verdict::Park`
  (`clank_core/turn_policy.rs:874`, `:1225`), which holds the Stop hook open, so the normal
  resting state of an auto-on Claude agent is mid-hook.
- The session binding is `config.json`'s `session {id, tool, updated_at, …}`
  (`core/agent_config.rs:758`), every writer serialised by the per-label `binding.lock` flock
  (`agent_store.rs:637-675`); `binding_matches` (:59) is the check. Its production holders
  (`update_agent_config` :86, :609, :695, `bind_session_to_agent` :727) are short config
  transactions that take no other lock and write no pane. A pane's identity is its launch
  provenance in the uid runtime dir, verified against the live process's pid and kernel birth
  token (`pane_target.rs:302-380`); `locate` (:41) fails closed on any doubt.
- File patterns to reuse: `plan_mutation::runtime_lock` (`plan_mutation.rs:26`, an `O_NOFOLLOW`
  0600 flock in `/tmp/clank-run-<uid>`, never unlinked); `agent_store::exclusive_lock` (:667);
  atomic temp-then-rename JSON writes (`agent_store.rs:410`); file-based request/result control
  with no socket (`remote_control.rs:1-8`); the `notify` watcher machinery (`repo_watch.rs`),
  whose allowlist covers `.clank/agents` (:29), a namespace agents routinely edit. Nudge history is `nudges.jsonl` (`nudge_log.rs`,
  `Trigger` :14). `monitor::repo_tag` (`monitor.rs:1796`) names a repo by a blake3 of its path;
  `monitor::trusted_dir` (:559) vets a directory's owner, mode and symlinks.
- Typed recovery and check-ins exist only where `recovery::transport_for` answers
  `Transport::Typed` (`recovery.rs:2036`, `:2062`), which is Claude and Codex. Grok has no resume
  candidate (`resume.rs:31`), and OpenCode resumes through its plugin. So Grok and OpenCode panes
  get terminal text today only from `clank nudge`, the event-wake fallback and the web `say`.
- Crypto: `ring 0.17.14` is in `Cargo.lock` (:3557, via rustls, rustls-webpki, quinn-proto) and
  verifies Ed25519. No `ed25519-dalek`. `base64 0.22` and `blake3` are already direct deps of
  `crates/cli`. The only addition is `ring = "0.17"` as a direct dependency: no new crate in the
  lock.
- User config is `~/.clank/config.json` as the typed `UserConfigFile`
  (`teams_config.rs:44`, read by `team.rs:55`). Clank no longer writes Claude permission rules
  (`init.rs:333` retires them), so deny rules are the operator's, documented in the README.
- Harness triggers, read from the installed binaries: Claude Code 2.1.308 switches to bash mode
  when an insertion at cursor offset 0, or into an empty input, makes the value start with `!`
  (`cb`: `startsWith("!")`, untrimmed; `Wcs`), consumes that `!`, and draws `!` in place of the
  `❯` pointer with a `bashBorder` rule. Codex 0.163.0: "Prefix a command with ! to run it
  locally" (`input_submission.rs`), with a "Shell mode" composer state (`chat_composer.rs`) and no
  documented key that leaves it. Grok 1.0.62: "type ! on an empty prompt"; "Delete the leading ! to
  go back to a normal prompt"; stash-restore and history recall re-enter it. Its Ctrl+C "with a
  draft, the first press only clears it", and Ctrl+U is undo, not a line-kill. OpenCode 1.18.30: a
  `!` at offset 0 in normal mode enters shell mode. Its prompt reducer returns to normal on Escape,
  or on Backspace in an empty input, and its UI string `prompt.mode.shell.exit` is "esc to exit".
  That code may be the desktop prompt rather than the TUI's, so it counts as a lead, not proof.

## 1. The request and its proof

The site holds an Ed25519 private key; clank holds only the public key, as
`user_command.site_key` in `~/.clank/config.json`. Clank contains no signing code and never reads
the private key or accepts a path to one.

- **Provisioning.** The site generates the keypair and keeps the private key in a 0600 file of its
  own. The user runs `clank user-command trust-key`, which reads the public key from stdin, writes
  it through the typed user-config writer, and prints its blake3 fingerprint. Replacing an existing
  key needs `--replace`.
- **Submission.** `clank user-command submit` reads one JSON envelope from stdin, never argv:
  `{request, sig}`, where `request` is the base64 of the payload bytes and `sig` the signature over
  a fixed domain prefix followed by exactly those bytes. Clank verifies the signature before
  parsing anything, then parses the bytes it verified. This refines "canonical bytes": the signed
  bytes are the transmitted bytes, so there is no re-serialisation to disagree about.
- **Payload.** `{v, repo, agent, session, tool, id, command, issued_at, expires_at}`. Every field
  is signed, so changing any one is a forgery. `repo` must equal the canonical path; `agent` must
  be on the roster; `session` and `tool` must equal the current binding; `tool` must be `claude`.
  `id` is a bounded `[A-Za-z0-9_-]` token, safe as a filename. `command` is one non-empty line of
  bounded length with no control characters, the bound set by the live evidence row.
  `expires_at - issued_at` is at most 10 minutes; clank checks `now` against both, with small
  clock skew, at admission and again at dispatch. A command that waits past its expiry is refused
  as expired; the site re-signs on a new press, with a new id.
- **Output.** `submit` and `clank user-command status --repo <r> --agent <a> --id <id>` print the
  record's state as JSON (`id`, `state`, `reason`, timestamps, key fingerprint). Neither prints the
  envelope, the signature or the command. No credential goes into argv, logs or results.
- **What it protects.** An agent cannot forge a request: it has the public key only. A captured
  envelope (from a log or a message) authorises exactly one command, to one target, under one id,
  and any id that already has a ledger entry is never dispatched again. Submitting a captured
  envelope before the site does delivers the command the person chose, once. A swapped trusted key
  makes the site's own requests fail verification, which the site sees.
- **Key replacement or revocation.** Trust is judged when a record is used, not when it was
  admitted. A record whose envelope no longer verifies under the current `site_key`, after
  `trust-key --replace`, or with no key at all, is refused (`key changed` or `no trusted key`)
  the next time the monitor would unpark or dispatch for it.
- **What it does not.** Same UID. Any of these defeats it:
  - reading the site's private key file;
  - writing `~/.clank/config.json`, or running `trust-key --replace` with a key of its own;
  - modifying or deleting the attempt ledger under `~/.clank/user-commands/` (section 2), which
    defeats replay prevention: a genuine envelope whose entry is gone or reset can be dispatched
    again until it expires.

  Deny rules match tool calls, so they cover `Read`/`Edit`/`Write` of those paths and the obvious
  Bash spellings, not every Bash path to the same bytes. The README states this and gives the deny
  rules to add: the private key's path, writes to `~/.clank/config.json` and to
  `~/.clank/user-commands/**`, and `clank user-command trust-key`.

## 2. The record, and at-most-once

The queue, the id locks and the attempt ledger live outside every repo, in a user-level namespace
that agents have no reason to edit: `~/.clank/user-commands/<repo_tag>/<label>/<id>.json`, with
`<id>.lock` beside it. Directories are 0700 and files 0600, opened `O_NOFOLLOW` and vetted with
`trusted_dir` before every read. It is durable, in the home directory rather than `/tmp`, so a
reboot inside an envelope's lifetime cannot reset replay state. It is files, not a socket. An
entry may be pruned only once its signed `expires_at` (plus skew) has passed, since an expired
envelope is refused anyway. That protects nothing against a same-UID writer, as section 1 says;
it keeps the ledger out of the namespace agents routinely edit.

- **What a record holds.** The exact envelope as admitted (`request` and `sig`), and clank's own
  state machine: state, reason, the pane observed, timestamps. It holds no unsigned copy of the
  command, target, id or times. Every use of a record re-verifies its envelope against the current
  trust policy (key and 10-minute window) and derives those fields only from the verified payload.
  The record is also refused if its payload's repo, label or id disagree with the path it was
  found at. A record fabricated or edited after admission therefore causes no unpark and no write:
  without a valid signature it is refused. The one thing a same-UID writer can still do is reset
  or delete the state of a genuine envelope, which is the replay limit stated in section 1.
- **States.** `admitted` → `waiting-for-idle(reason)` → `dispatching` → `delivered` |
  `unconfirmed(reason)` | `refused(reason)` | `failed-not-started(reason)`. The last four are
  terminal. `delivered` means the transport accepted the `!` line and Enter, not that the command
  succeeded; its output is in the agent's conversation.
- **Bound to its payload.** The same id with byte-identical signed payload returns the existing
  state (and asks the dispatcher to re-evaluate a waiting record). The same id with any other
  payload is refused as a collision, and the record is untouched. A request whose signature fails creates no record: an unauthenticated caller cannot
  squat ids.
- **Dispatch.** `dispatching` is persisted, and synced, before the first byte is written, and the
  id lock is held across the whole attempt. Then the outcome is persisted: both writes accepted →
  `delivered`; `PaneWrite::NotStarted` on the first write → `failed-not-started`; any `Unknown` →
  `unconfirmed`.
- **After a crash.** A reader that takes the id lock and finds `dispatching` knows its dispatcher
  died (a flock dies with its holder) and converts it to `unconfirmed`. It is never replayed. A
  reader that cannot take the lock reports `dispatching` as it stands.
- **One dispatcher.** Only the monitor moves a record out of `waiting-for-idle`. `submit` admits
  and reports; with no monitor running a record waits, and its state says why.
- **Seam.** The dispatch takes the terminal write and the record store as supplied functions, in
  the style of `submit_with`, so tests drive it with a fake writer and a fake crash, never a real
  pane.

## 3. Target and readiness

- **Target.** The record pins repo, label, session and tool. The first time the monitor looks, it
  also records the pane it found (`pane_target::discover_process`: session, pane, pid, birth).
  At dispatch it is refused, needing a new authorisation, if the binding no longer matches, the
  pane cannot be located with certainty, the launch process is not verified, or the pane's
  pid/birth differ from the recorded ones (a replaced pane).
- **Eligibility, checked at dispatch on the monitor's loop, in its last look:** the agent's
  anchored latest turn is `Completed`; custody has no outstanding presentation and holds no parked
  hook; no question, block or permission prompt is pending; the launch is verified; and one pane
  dump under the pane lock reads Claude's input as `Empty`. That means the last `❯` line bounded
  by `───` rules above and below, with nothing under it but the dim `Try "…"` placeholder. It is
  ineligible on `Draft`, on the new `Composer::Shell` (the bounded input line opens with `!`
  instead of `❯`), and on `Unreadable`, which is what a permission dialog or a covered or cut-off
  input reads as.
- **Parked agents (a refinement the code forces).** Because an idle auto-on agent rests in a held
  Stop hook (`Verdict::Park`), "idle with no outstanding turn" is never true of it. When a park is
  the only thing between a waiting record and eligibility, and that record's envelope has just
  verified under the current key and window, the monitor answers that park with a new
  terminal `SilentWhy::UserCommand` verdict, so the turn ends with no continuation, and then
  evaluates again on the turn-end it sees. After the command runs, the agent sits unparked; work
  owed later reaches it through recovery's existing idle nudge, as for any agent that stopped with
  no wait. Whether a bash-mode command fires a Stop hook is recorded in the evidence row, and the
  design depends on neither answer: a Stop is an ordinary turn-end and parks again.
- **Wakes, not polling.** Re-evaluation runs on the monitor's existing state changes (turn-end
  reports, park answers, recovery record reads) and on a file watch of the repo's
  `~/.clank/user-commands/<repo_tag>/` directory, using the same `notify` machinery as
  `repo_watch` (not a socket). The pane is dumped only when every other fact already says go. A record that waits on
  a draft waits until the next state change, its own expiry, or a repeat submit of the same
  request.
- **The keystrokes.** One `write-chars -- !<command>` into the empty input (Claude consumes the `!`
  as the mode switch), then Enter as byte 13. This is a private writer beside `submit_with`, never
  reachable with ordinary text.
- **Residual race, stated plainly.** The pane lock serialises clank's own writers. It cannot stop
  a person, or a process outside clank, typing between the dump and the write. In that window the
  person's keystrokes and the command can mix. The result is `delivered` or `unconfirmed`, never
  retried, and the history shows what was sent.

## 4. One pane write lock

Every terminal write takes one cross-process flock per pane before it looks or types. That is
every pane_input entry point, ordinary or user-command, including `stop_pane`. The lock is
`/tmp/clank-run-<uid>/pane-<blake3(session, pane)>.lock`, opened like
`plan_mutation::runtime_lock` but keyed by pane rather than repo. It is held across the dump, the
check and every write of one sequence: interrupt and text and Enter are one hold. Async callers
take it off the executor, as `event_interrupt::blocking` does. The raw transports become
private to pane_input's use: the
`zellij_ownership_boundary` gate gains a rule that only `open_zellij.rs` and `pane_input.rs` name
`write_pane_action*`.

**Lock order, and the one commit point.** A dispatch takes the id lock, then the pane lock, and
with both held:

1. takes one dump, which must read Claude's input as `Empty`;
2. asks the monitor loop for its readiness answer (turn completed, no park, nothing outstanding,
   no question or block);
3. takes the label's `binding.lock` (through `agent_store::BindingGuard`);
4. re-reads the trusted key and the clock and re-verifies the envelope;
5. checks that the binding read under the guard still equals the signed session and tool;
6. checks that the launch provenance still matches the recorded pid and birth, and the evidence
   row;
7. persists `dispatching`;
8. types.

Steps 2 to 8 are the commit point: everything decided before the dump or before the pane was
reserved is decided again after them, so a request that expires or loses its key while the dump
is pending, or an agent another clank writer started a turn in before this attempt won the pane,
is not typed. Locks are released in reverse.

No cycle. The loop blocks on none of the attempt's locks: it takes no `binding.lock`, takes id
locks only with a try-lock, and never takes a pane lock. Its own pane writers (recovery's typed
sends, check-ins, event wakes) run off the loop and take the pane lock with a non-blocking
try-lock that defers (`Unavailable`, event delivery left pending) when the pane is reserved, so
they also never type on an authorisation made before a user command ran. So asking the loop while
holding the id and pane locks cannot wait on this attempt. `binding.lock` holders take no other
lock, and ordinary writers outside the monitor (`clank nudge`, the web bridge) wait only for the
pane and hold nothing while they do. Clank's own rebinds commit under `binding.lock`, so none can
land between step 5 and the last byte; that race is distinct from the human-input race in
section 3. The `binding.lock` hold is bounded by the two 3 s writes, so a SessionStart rebind can
wait up to about that long.

## 5. The ordinary-text admission rule

pane_input gains one constructor for ordinary text, a newtype that every ordinary entry point
takes instead of `&str`. The native paths take it too (`codex_chat::send`,
`codex_interrupt::try_wake`, the OpenCode plugin prompt), so no caller has to prove a provider API
ignores `!`. The user-command writer is the only pane writer that takes a raw line. The rule:

- **Structural bytes.** CRLF and lone CR become LF, because CR would submit mid-message and the
  next character would start on an empty prompt. LF stays: it is the existing multi-line
  mechanism, and content is never flattened. Tab becomes a space, since Tab is Codex's submit and
  queue key. Every other C0 control, ESC included (Esc interrupts, a CSI sequence can be the kitty
  Enter), and DEL and C1 controls are replaced by U+FFFD, so the change is visible.
- **A leading `!`.** If the text, after its leading whitespace, starts with `!`, it gets the safe
  encoding for its tool. For Claude, the leading whitespace becomes a single ASCII space: 2.1.308
  tests the untrimmed value with `startsWith("!")` and switches only on an insertion at offset 0.
  This is confirmed by a live evidence row before it is used. For every other tool, or a Claude
  version without a row, a fixed non-whitespace prefix is used: U+00B7 `·` before the `!`. Every
  tool's trigger is a `!` as the input's first character, and a non-whitespace character cannot
  be trimmed away. Evidence rows live with the encoding, in the style of `resume::EVIDENCE`.
- **A pane already in shell mode.** Once shell mode is active, ordinary text needs no `!`, so the
  encoding above cannot help. Under the pane lock, an ordinary terminal write needs one of two
  things, judged per tool and per path, or it is refused:
  - **A positive reading.** One dump reads the input as not in shell mode. For Claude that is
    `Empty`, or `Draft` on a `❯` line; it is refused on `Shell` (the bounded line opens with `!`)
    and on `Unreadable`. For Codex it is `Empty`, or a `Draft` whose typed text does not begin
    with `!`; it is refused on `Shell` and on `Unreadable`. Codex has no proven exit key, and by
    its own help text its shell command is the line's leading `!`, so text appended to a `!git`
    draft would run.
  - **A proven exit key, sent first in the same locked sequence.** Grok's Ctrl+C clears a draft,
    the leading `!` that carries its shell mode included. OpenCode's Esc returns its prompt to
    normal. Each is the key those tools' interrupt-first paths (`clank nudge`, the event-wake
    fallback) already send first (`stop_argv`), so those paths stay usable, but only behind a
    committed evidence row at the installed version. A Grok row must cover a bare `!` with nothing
    after it; an OpenCode row must show Esc in shell mode leaves it, and whether it also
    interrupts. Without its row, the tool is refused on these paths.

  A path with neither is refused with `Unavailable`. In practice:
  - **Grok and OpenCode.** The web `say`, which sends no interrupt, is refused (today it types into
    them as if they were Claude). Recovery and check-ins change nothing, because neither tool is
    typed to today: Grok has no resume, and OpenCode resumes through its plugin.
  - **How a refusal surfaces.**
    - `clank nudge`: a `Refused { reason }` line in `nudges.jsonl` and the command's error.
    - Event wake: `Unavailable` (`event_interrupt.rs`), which logs the reason and leaves normal
      delivery pending.
    - Recovery and check-ins: `Unavailable`, as on any failed last look.
    - Web `say`: an error to the caller.
  - **Native paths stay usable,** because no TUI composer parses their text: the Codex app-server
    (`codex_chat::send`, `codex_interrupt::try_wake`), the OpenCode plugin resume, and Grok's
    native hooks. They still take the admitted-text type.
- The web bridge resolves the pane's real tool for `say` as `stop` already does.
- Out of scope: other first-character modes (`/` commands and the like) are pre-existing Claude
  behaviour, run under the agent's own checks, and are not made reachable by this plan.

## Tools

Claude Code only, for the user command. Codex has a shell mode ("Prefix a command with ! to run
it locally"), but clank has no verified rendering of it, its idle submission is a different key,
and its typed path queues behind turns. Grok and OpenCode have shell modes but no readable input
line. All three are refused with that reason. The admission rule covers all four for ordinary
text.

Live evidence rows, observed in throwaway panes against the installed harnesses, are committed
with the implementation.

- **Claude:**
  - the user-command keystrokes running one line (quotes, `$`, and a long line at the chosen bound);
  - what `Shell` looks like in an `--ansi` dump;
  - that a space-prefixed `!` stays a prompt;
  - that a `!` after an LF inside a multi-line message stays text;
  - whether a bash-mode command fires a Stop hook.
- **Codex:** what its shell mode looks like in a dump.
- **Grok and OpenCode:** whether the exit key leaves shell mode, as in section 5.

## Tests

Through the real seams, with fake writers and fixtures, no real panes:

- **Credential:** a forged signature, each signed field changed in turn, an expired request, one
  issued with a window over 10 minutes, no trusted key, a malformed envelope, and `submit` with no
  proof. Each one creates no record and writes nothing.
- **Stored authority:** a record whose stored payload, signature, target or times are edited after
  admission, and one fabricated outright, cause no unpark and no write. So does a record whose
  path disagrees with its payload. After `trust-key --replace`, or with the key removed, an
  admitted record is refused at its next use and nothing is unparked or typed.
- **Record:** a repeated id with the same payload returns the existing state, and the same id with
  a different payload is refused. Failure before the text gives `failed-not-started`; failure
  between text and Enter gives `unconfirmed`; a crash after `dispatching` and before the result is
  recorded, at each of the three points, gives `unconfirmed` on reload with no second write.
  Expiry at dispatch is refused.
- **Target:** refused on a binding change, on a replaced pane (pid/birth differ), on a non-claude
  tool, and when the pane is ambiguous.
- **Readiness:** dispatch only on `Empty` with a completed turn, nothing outstanding and no park.
  `Draft`, `Shell`, `Unreadable` and a running turn wait. A parked agent gets the
  `SilentWhy::UserCommand` answer before it is typed to.
- **Lock:** an ordinary writer and the user-command writer on one pane never interleave. A rebind
  taken during a dispatch waits for it, and a dispatch that starts after a rebind is refused.
- **Admission:** each caller class (nudge, event wake, recovery typed, check-in, web say, and each
  native path) is driven through its writing seam and shows the leading-`!` encoding, CR and ESC
  neutralised, and LF kept. The gate fails if a raw transport is named outside its owners.
- **Already-active shell mode,** for each tool and each path class:
  - Claude and Codex: refused on a shell composer, an `Unreadable` dump, and (for Codex) a `!` draft.
  - Grok and OpenCode: their interrupt-first paths send the exit key first only with an evidence row
    present, and are refused without one; the web `say` is refused.
  - Every refusal leaves event delivery pending and writes nothing.
- `composer` fixtures for Claude's and Codex's shell modes, from the evidence dumps.
- The repo's checks pass for the modules touched.

## Done when

**Committable, and inside the reviewer gate:** this plan, then one complete implementation commit
with the tests above, the evidence rows, and a README section covering the interface, the
credential and its limits, and the deny rules.

**Deliverable outside the gate:** the report to clank-wr. It gives the interface (`trust-key`,
`submit`, `status`, the envelope and payload, the states and reasons), the credential's design and
limits, every pane path and what it now requires before it types, and what each tool supports. Install, merge and activation, and
the live human smoke test are outside the reviewer gate. That smoke test is a throwaway Claude
agent, a site-signed `echo`, and the output in its transcript.
