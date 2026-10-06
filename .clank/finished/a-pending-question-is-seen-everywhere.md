# a-pending-question-is-seen-everywhere
# A pending question is seen everywhere

## Why

When Claude Code asks the user something mid-turn (its `AskUserQuestion`
tool), the turn is stopped until someone answers. Lloyd reports two gaps:

1. The web remote transcript did not show that Claude was asking.
2. `clank status` never says an agent is blocked on a question.

Much of the machinery already exists:
- `web/transcript/questions.rs` parses typed `AskUserQuestion` fields.
- `web/transcript.rs` attaches them to tool rows and correlates results.
- `page.html` renders question cards.
- `web/claude_questions.rs` arbitrates native answers through
  `clank question-hook`.
- `setup` installs that hook for `PreToolUse`, `PermissionRequest`,
  `PostToolUse` and `PostToolUseFailure` (confirmed installed in Lloyd's
  `~/.claude/settings.json`).

What is missing is a pending-question state that exists without a viewer:
- The hook's registry and feed are owned by the web bridge.
- `question-hook` returns empty when no broker is listening, and records
  nothing.
- The transcript card depends on when Claude writes the tool call to its
  transcript. The record's timestamp cannot tell a call written late from
  one written at the time.

So nothing outside an open page knows a question is pending: not status,
not the TUI, not recovery. Gap 1 is therefore either that timing, or no
page being attached. The first step below decides which, with committable
fixtures.

## What to build

- **One owner: the hook that already runs.** `clank question-hook` runs on
  every `AskUserQuestion` in every Claude session, with or without a
  broker. Before arbitration, it records the question's lifecycle in the
  bound agent's directory (`question.json`):
  - written on `PreToolUse`;
  - updated on `PermissionRequest`;
  - resolved by the matching `PostToolUse` or `PostToolUseFailure`.

  Status, the TUI, the remote status projection and recovery all read that
  one record. No surface keeps its own state machine, and nothing is added
  that polls or helps.
- **Separate from the answer channel.** Recording a pending question never
  claims there is a live remote answer request. The native answer channel
  keeps its own authority, and a viewer attaching or leaving never creates
  or clears the record.
- **Identity.** A record is keyed by the native session, `prompt_id` and
  `tool_use_id` from the hook payload.
  - `PermissionRequest` carries no `tool_use_id`, as the native fixture
    asserts. It is matched to its record by the existing correlation in
    `claude_questions.rs` (one current pre-tool event with the same session,
    prompt and tool input). No tool id is invented.
  - A `PermissionRequest` with no candidate, or more than one, changes no
    record. The native answer channel's authority is unchanged.
  - Only the result or failure with the same key resolves it.
  - A hook from another session (old, rebound, forked or cleared) neither
    clears nor overwrites the bound session's record, and is ignored unless
    its session is the one bound.
  - Duplicate or out-of-order observations are idempotent. A result that
    arrives before its pre-tool event leaves nothing pending.
- **Terminal transitions, from one reducer.** One reducer in the agent's
  directory applies every observation to the record, scoped to its
  originating session and prompt. Each ending names the signal that covers
  it, because no single one covers all of them:

  | Ending | Signal |
  | --- | --- |
  | Answered, locally or remotely, or dismissed | The matching `PostToolUse` |
  | Tool failed | The matching `PostToolUseFailure` |
  | Turn ended normally | The Stop hook for that session, with a newer or ended prompt |
  | Cancelled in the terminal, which runs no hook and skips Stop | The call's rejected result in Claude's own record |
  | Turn failed on a provider error, which skips Stop | The failed turn in Claude's own record, as the recovery reader already reads it (`isApiErrorMessage` for that prompt). Its observations feed the same reducer. |
  | Superseded | A newer `promptId` in that session's record |
  | Session ended, cleared or replaced | SessionStart, and a change of binding seen by the monitor's existing fold |

  - The ended prompts and returned calls of the bound session are kept,
    so a late or replayed `PreToolUse` cannot bring a question back.
  - None of this adds polling. The native record is read on its existing
    watch events while the agent owes work or has a question pending; for
    a question alone, nothing is decided from it.
- **A durable, shared projection.** The record lives on disk, so neither a
  monitor restart nor a viewer leaving changes it.
  - **Restart:** a question still waiting after a restart stays pending,
    and needs no further hook. On restart the reducer reconciles the record
    with the bound session's native record: a terminal event found there (its
    result, a newer prompt, a failure, or the session no longer bound)
    retires it. The record is read back once, as far as the question's
    prompt, however old it is.
  - **Corrupt or unreadable:** a record that cannot be read is treated as
    unavailable. Status shows nothing, recovery does not hold, and it is
    never called answered.
  - **Bound session's record unreadable:** a valid record stays pending
    until a hook or binding change ends it.
  - No expiry by age, and no polling.
- **Recovery holds while a question is pending.**
  - The stall watchdog and failed-turn recovery treat a pending question
    like attendance: no stall notice and no retry while it is pending.
  - They resume when it is resolved or retired.
  - Genuine `clank block`s stay separate, and required-reply events are
    still delivered.
- **Status.** `clank status`, the TUI and the remote status projection
  show `<agent>: asking you — "<header>"`.
  - The header is the question's own header, or else the first line of its
    text.
  - Clearing needs no page refresh and no recurring scan.
- **Web transcript.** The existing card stays. If the timing check shows
  Claude writes the call only after it is answered, the remote view also
  shows the pending question from `question.json`, the same record status
  reads, until the transcript catches up. The existing question rendering
  and native answer controls are reused unchanged.
- **Timing check, in fixtures.** Using the existing protocol fixtures,
  record and state when the hook events and the transcript's tool call
  each become visible, and therefore which source each surface relies on.
  This documents evidence; it is not a manual gate on completing the plan.

## Acceptance

- **Headless:** with no web bridge, a recorded `PreToolUse` makes
  `clank status` and the TUI show the agent asking. The stall watchdog and
  failed-turn recovery both hold.
- **Resolution:** the matching `PostToolUse` (local, remote or dismissed)
  or `PostToolUseFailure` clears it, and recovery resumes.
- **Late attachment:** a viewer opened after the question is pending sees
  it. Attaching or leaving changes nothing.
- **Identity:**
  - A late result from an old session after a rebind does not clear the
    new session's record.
  - Another session's pre-tool event does not create one.
  - Duplicate or out-of-order hooks are idempotent.
- **Endings:** a turn interrupted, or a session ended or replaced, with no
  matching result retires the pending state.
- **Restart:** a real outstanding question survives a monitor restart
  with no further hook, still shown and still holding. A question that
  ended while the monitor was down, without its result hook, is retired on
  restart from the native record.
- **Native failure:** a provider failure of the question's turn, with no
  `PostToolUse` and no Stop and the binding unchanged, clears the hold.
- **`PermissionRequest`:**
  - matched unambiguously, it updates its record;
  - with no candidate or several, it changes nothing;
  - a late `PreToolUse` cannot revive a retired record.
- **Corrupt record:** shown as nothing, and never holds or reads as
  answered.
- **Unchanged:** typed question content and the native answer controls
  still work. A Codex, Grok or OpenCode agent's status and transcript are
  unchanged.

## As built

- **The reducer.** `cli::question_state` is the reducer (`apply`) and the
  durable projection (`question.json`), written under `question.lock`.
  - The projection is about one session: the agent's Claude binding, read
    under that lock on every write. An observation of any other session is
    a stale producer's and changes nothing. A new binding drops everything
    of the old session, since none of it can be accepted again; that is
    the only compaction.
  - For its session it keeps the pending questions, every prompt known to
    be over (whether or not anything was asked in it), and the calls that
    returned in prompts not yet over. An `Asked` for an over prompt or a
    returned call is refused, in any order and however often replayed.
- **Producers.** Each feeds the same reducer:
  - **`clank question-hook`** records `PreToolUse`, `PostToolUse` and
    `PostToolUseFailure` before any broker is asked.
  - **The Stop hook** ends the turn its transcript proves ended (the newest
    prompt read at hook entry) and the prompts its last 256 KiB show
    behind it.
  - **The recovery driver** reads the record of every agent that owes work
    or has a question pending. For an agent that only has a question, it
    reads and decides nothing. Each read gives, in the record's order:
    - the turns it saw begin, which are behind the newest; this is
      collected as lines are read, so turns that leave the snapshot
      within one read still count;
    - a pending call's recorded result: a question cancelled in the
      terminal runs no hook and no Stop, so this is its only end;
    - a failed newest turn.
    When the watch is armed (a start, a restart, a new record), each
    pending question is also looked for in the record by `recorded_end`.
    That reads back from the end in widening stretches until it holds the
    start of the call's prompt, looking for the call's result or a newer
    prompt. It happens once, so a question older than any window is still
    retired.
  - **The monitor fold** settles the projection on the binding, and tells
    recovery which agents are asking.
- **Publication timing**, from the protocol fixture against Claude 2.1.280:
  - When `PreToolUse` and `PermissionRequest` run, the transcript holds
    neither the prompt nor the call.
  - The call is published soon after, while the question still waits;
    this is asserted, with a 10 s bound.
  - So the transcript card is a viewer's question card. The status fact
    covers the moment before it appears, and nothing needs a second card
    or deduplication.
  - Since a hook can precede its prompt in the record, the absence of a
    prompt is never taken as evidence.
- **`PermissionRequest`** changes nothing in the projection. Its
  correlation stays with the answer channel.
- **Readers.**
  - The recovery gate, and so the stall watchdog and failed-turn
    recovery, holds for a pending question.
  - `clank status` shows `asking you: <agent> → "<header>"`, and the TUI
    shows it as blocked but not openable.
  - The remote page reads `asking` from the status facts and says
    `asking you "<header>"` in the header and the picker, to a viewer who
    connects at any time.
  - An unreadable record or binding shows and holds nothing, and the
    record is never overwritten.
- **Proof.**
  - The installed end-to-end test (real Claude, the real `clank
    question-hook`) asserts:
    - the question is recorded;
    - remote and local answers end it through the hook;
    - a cancel runs no hook, and the real recovery reader ends it from
      the real transcript.
  - The browser check covers the header and the picker.
  - `the_status_frame_says_a_recorded_question_is_waiting` follows the
    hook through to the status frame.
