# the-baton-shows-whose-turn-it-is

> Have the possibility of both: the clean JSONL approach or the raw
> one from zellij subscribe — two display modes. Render one agent per
> page. Have a sidebar/tray that should show something like
> `clank status --tui`; switching agents is done there. The sidebar
> is a suggestion — be creative, make something efficient and clean
> with good visibility on whose turn it is and what's being done.
> — lloyd

## The subject is a relay

Work in a clank repo passes hand to hand in a fixed order — the
master implements, the reviewers verdict, the master again — and the
gate always knows exactly who owes what. That is a TURN model, and
the page should be built on it rather than on a list of agents.

One correction to the metaphor before it misleads the build: a relay
has one baton, but a turn here can be held by SEVERAL agents at once.
`ReviewerApprovalsMissing { missing }` and `GateReviewersMissing` carry
a `NonEmptyVec` — codex and ruthless both reviewing the same commit is
the ordinary case, and the TUI spins both rows. And the turn is not
only the active plan's: `in_progress_rows` and `awaited_reviewers`
already union plan, PR and ad-hoc work through the one `is_actionable`
routing, and a plan-only reading of `waiting_on` would mark an awaited
ad-hoc reviewer idle (codex on 5477c38). So the page reads WHO HAS THE
FLOOR from those shared facts — one agent or many — never from
`waiting_on` alone, and the hero is written to be plural.

The current PoC shows each agent's terminal well enough and says
nothing about whose turn it is beyond a breathing dot. This plan
makes the turn the hero, gives every agent a second way to be read,
and moves everything that is history into one place.

## What the harnesses actually keep (measured on this machine)

`zellij subscribe` hands us rendered cells — the output of each
tool's TUI, not the conversation. Two of the four harnesses keep the
conversation itself, in a shape clank can read:

- **Claude Code** — one JSONL per session at
  `~/.claude/projects/<cwd slug>/<session id>.jsonl` (this session's:
  407MB, 269k lines). The conversation is the `user` and `assistant`
  lines; every line carries a top-level `timestamp` (ISO), `uuid`,
  `parentUuid`, `isSidechain`, and `message`. An `assistant` line is
  ONE content block — `text`, `thinking`, `tool_use` — so a message
  is several lines sharing `message.id`, appended as each block
  completes. A `user` line's content is one of four shapes: a plain
  string (the person spoke); a string wrapping `<system-reminder>`
  (the harness spoke — a stop-hook nudge — and must not render as
  the person); a list of `tool_result` blocks keyed by `tool_use_id`;
  a list of `text` blocks (harness notes such as an interruption).
  Everything else in the file (`permission-mode`, `queue-operation`,
  `file-history-snapshot`, …) is bookkeeping. The Stop and
  SessionStart hooks hand clank `transcript_path`; clank currently
  drops it.
- **Codex** — one rollout JSONL per session at
  `~/.codex/sessions/YYYY/MM/DD/rollout-<stamp>-<session id>.jsonl`,
  the id in the filename and in the first line's `session_meta`
  (penlock's master: 1.47GB). The conversation is `response_item`
  lines, each with a top-level `timestamp`: `message` (`role`,
  `content: [{type: output_text | input_text, text}]`),
  `custom_tool_call` (`name`, string `input`, `call_id`) and
  `custom_tool_call_output` (`call_id`, `output` as blocks or a
  string), `function_call`/`function_call_output` (the same pair with
  `arguments`), and `reasoning` (`summary`, usually empty, the rest
  encrypted). Its `event_msg` stream has `item_completed` and no
  deltas: the same per-completed-item granularity as Claude.
- **opencode** (kimi) keeps a SQLite store and is client/server by
  design; **grok** has a session store under `~/.grok/`. Neither
  shape is read in this plan; those agents get the terminal only,
  and say so.

So a native transcript is near-live — a paragraph lands when it is
finished, not token by token — and that is the right granularity for
the phone's question, "what did it say, and is it done". What is
happening RIGHT NOW (a spinner, a permission prompt, streaming text)
is the terminal's to show, which is why both modes stay.

## The design

Grounded in the relay. One bold thing, everything else quiet.

**The floor is the hero.** One sentence, set large, naming everyone
who holds the turn right now and what each owes:

    codex  is reviewing 3976e7f      4m

Names in Martian Mono 700 in the phase hue; the verb in italic; the
object (a short sha, a plan, a block) plain; the elapsed inline at
the name's size, because "four minutes or forty" is the number a
supervisor actually wants, and a stat tile would be the default. It
is the TUI's own facts — `in_progress_rows` says who and with which
verb, the `Handover` dates each (the `since` the TUI derives),
`state_color` hues it — so it covers exactly what the TUI covers,
plan or not. With several holders the sentence lists them, each with
its own clock, oldest first:

    codex and ruthless  are reviewing 3976e7f     4m, 2m
    claude   is working on a-tabs-shape…          12m
    claude   is revising after codex               1m
    claude   is drafting a-tabs-shape…             3m
    claude   is finalizing a-tabs-shape…           1m
    codex    is reviewing 8a1f0c2  (ad-hoc)       2m
    ruthless is reviewing PR #61, round 2          6m
    you      are asked  "Should we …"              2h    (blocked — red)
    claude   must fix HEAD's tag                        (orange)
    nobody   — idle since the finish               3h

Every `WaitingOn` variant and every plan-less routing has a line:
`ReviewerApprovalsMissing` and `GateReviewersMissing` (several names),
`MasterToContinue` / `MasterToRevise` / `MasterToCommit` /
`MasterToFinalize` (the TUI's four verbs), `Blocked` (you),
`MasterToFixCommitTag` and a head correction (no clock — nothing was
handed over, something is wrong), ad-hoc review and revision, PR
reviewer and PR master, and the empty case. The verbs are `verb_of`'s
and the four the ad-hoc and PR arms already use; nothing is coined.

**The track replaces the sidebar.** Under the baton, the agents sit
as positions on one row in CYCLE order — master first, then the
reviewers — each with its role and its last act:

    claude        codex ●        ruthless
    master        commit         final
    ✎ committed   reviewing 4m   ✓ continued, 12m ago
    ▔▔▔▔▔▔

Two marks for two independent facts: a breathing dot in the phase
hue on EVERY agent that holds the floor — one or several — and an
ink underline on whoever you are LOOKING AT. Tapping a position
selects it; the floor never moves by tapping. On a phone the row
scrolls sideways. That one row is both the who's-turn display and
the switcher, so the page needs no sidebar for either.

**One agent, two modes.** A header with the selected agent's name and
a text-only switch, `transcript | terminal`. Transcript: the turns in
a column no wider than 80 characters, a narrow gutter carrying `you`
or the agent's name and the turn's age at the TUI's coarse ladder;
tool calls as one collapsed line — `▸ Bash  cargo test -p clank` —
opening to input and output on tap; thinking collapsed and dim;
harness noise not shown. Terminal: the xterm box exactly as now. The
mode is remembered per agent in the browser; the default is
transcript where one exists and terminal where not, with the switch
saying why: `no transcript for kimi yet`. The say box stays at the
bottom, addressed to the shown agent.

**The ledger holds what has been done.** The plan name and the gate
word in its hue; dirty; queue and stash counts; then the TUI's own
log rows — plan umbrellas, `⚑ sha subject`, `✓ codex: lgtm` — with
ages, in the TUI's order. On a desktop it is a fixed 36-column column
on the right; on a phone a drawer under `≡` at the top. A pending
block is shown here in full, with the sentence that a block is
answered at the terminal or with `clank block answer` — answering it
from the page is not this plan.

**Tokens.** Base `#1a1917`, bezel `#242220`, ink `#e8e4dc`, muted
`#8a857c`. The one accent is the TUI's phase hue — `Hue::hex` from
the last plan — and it changes meaning with the state, as the TUI's
bar does. Martian Mono 700 for names; the terminal's own face for
everything else. Left-aligned throughout. Motion: the baton's dot
breathes while someone is working, and the underline slides when the
turn passes — motion that shows what changed — and nothing else moves.

**Reviewed against the generic defaults.** The amber accent from the
last pass is gone: a second colour vocabulary beside the TUI's hues
was decoration. The suggested sidebar-with-status was the SaaS
default; the content splits naturally into live (the track) and
history (the ledger), and each goes where it is read. A big number
with a small label is the stat-tile default; the elapsed lives inside
a sentence instead. No caps, no eyebrows, no cards.

## The build

### Transcripts

- **`cli/web/transcript.rs`** — the normalized shape both harnesses
  reduce to, and nothing more:

      Turn { at: i64, who: Who, body: Body }
      Who   = Person | Agent | Harness
      Body  = Text(String) | Thinking(String)
            | Tool { name, input, output: Option<String>, id }

  One adapter per harness, pure over a line: `claude::turn(line)`
  and `codex::turn(line)` return `Option<Turn>` or a tool OUTPUT to
  attach to an earlier tool by id. Claude: `isSidechain` lines are
  subagent traffic and are skipped; a `user` string wrapping
  `<system-reminder>` is `Harness`, not `Person`; a `tool_result`
  attaches to its `tool_use`. Codex: `input_text` on a user message is
  `Person`; `reasoning` with an empty summary is skipped; `function_*`
  and `custom_tool_*` are one `Tool`. Fixture lines are shaped from
  the real ones on this machine with the content redacted.
- **Where the file is.** `Session` in core gains
  `#[serde(default)] transcript: Option<String>`. Claude's SessionStart
  payload carries `transcript_path`; `SessionStartInput` gains the
  field and the binding records it. A binding without one falls back
  to the derivation (`~/.claude/projects/<slug of cwd>/<id>.jsonl`).
  Codex: the one file matching `rollout-*-<id>.jsonl` under
  `~/.codex/sessions`; none means no transcript, several means the
  newest by modification time. Resolution is a pure function over
  (tool, id, recorded path, cwd) plus the glob, tested.
- **Tailing.** One thread per transcript. At start it reads the LAST
  window (a few MB — the files are hundreds of MB and more) and keeps
  the last N turns; then it follows growth, reading only what was
  appended, parsing whole lines. A rewrite (compaction, a file
  replaced) is detected by the size shrinking and handled by
  re-reading the window.
- **Identity, so a replay cannot duplicate.** A viewport frame is a
  full replacement, which is why the snapshot-then-live handoff may
  deliver one update twice without harm. A turn is not: an anonymous
  text replayed from the snapshot AND arriving on the stream would
  render twice, and every reconnect would repeat the window (codex on
  5477c38). So every `Turn` has a stable id from its harness — the
  line's `uuid` for Claude, the item's `payload.id` for codex — and
  the protocol is upsert, not append:
  - a `turns` frame is a REPLACEMENT of the retained window for one
    agent: `{ agent, session, generation, turns: [...] }`; the page
    swaps that agent's list for it.
  - a `turn` frame is an upsert of one turn by id:
    `{ agent, session, generation, turn }`; the page replaces the
    turn with that id or appends it. A tool's output arrives as the
    same turn re-sent whole, output attached, under the tool's id —
    there is no separate "attach" message to get out of order.
  - `session` is the binding the transcript belongs to;
    `generation` starts at 1 and increments when the tail re-reads
    the file (a shrink, a replacement) or the agent is rebound to a
    different session. The page drops any frame whose `session` or
    `generation` is not the current one for that agent, and clears
    the agent's list when a `turns` frame arrives with a new
    generation.
  A late browser gets each agent's `turns` frame after the table and
  before live; a lagged one is resynced the same way, and because the
  frame replaces rather than appends, a resync is only ever
  redundant.

### The baton, the track, the ledger

All three are projections of the snapshot the TUI already computes,
made inside `status_tui` where its `pub(super)` derivations are
reachable — the rule from the last plan. `web_facts` grows:

- `floor: Floor { holders: Vec<Holder { label, verb, object, since }>,
  ask: Option<block>, correction: bool, hue }` — the holders are
  `in_progress_rows` (which already unions plan, PR and ad-hoc work
  and carries `since`), the object is the sha, plan, PR or block the
  routing names, the ask is the pending block, the correction is
  `head_correction`, the hue is `state_color`. Empty holders and no
  ask is the idle line. It is the bar's `bar_text` as structured data
  plus the plural the bar cannot show.
- `track: Vec<Position>` in cycle order (master, then reviewers in
  roster order): label, role, `holds: bool` (in the floor's holders —
  any number may), its last act — the newest review it wrote (verdict
  and age) or the newest commit if it is the master — from the log
  rows and reviews the snapshot holds.
- `ledger: Ledger { plan, gate, hue, dirty, queue, stash, blocks,
  rows }` where `rows` is the TUI's `OnelineRow` list projected to
  `{ kind, sha, text, author, verdict, age }` with `row_age` — the
  ledger reads in the TUI's order because it IS the TUI's list.

`web_facts` is the one function, and the test that it agrees with
`bar_text`, `in_progress_rows` and the rows stands.

### The page

`page.html` is rewritten to the design: baton, track, one agent with
the mode switch, say box, ledger drawer/column. The terminal mode's
xterm handling — `screen` frames, the repaint, `panes` lifecycle —
carries over unchanged. Availability of transcript mode per agent
comes in the `panes` table (`transcript: bool`).

## Tests

No binary spawned, as ever. The pure parts:

- Adapters, on redacted real-shaped lines: Claude — a person's
  string; a system-reminder string is `Harness`; a `tool_use` then its
  `tool_result` become one `Tool` with output; `thinking`;
  `isSidechain` skipped; a bookkeeping line yields nothing. Codex — a
  user `input_text`; an assistant `output_text`; `custom_tool_call`
  then its output by `call_id`; `function_call` likewise; a
  `reasoning` with an empty summary yields nothing; `token_usage_record`
  yields nothing. Both: the `at` is the line's `timestamp`, and a
  future item type yields nothing rather than an error.
- Path resolution: Claude with a recorded path uses it; without one,
  the derivation; Codex with one match, none, two (newest wins).
- The tail: a file with M lines yields the last N turns at start; a
  line appended afterwards arrives; a partial line (no newline yet)
  is not parsed until complete; a shrink re-reads.
- Identity and replacement, the contract codex asked for: a turn
  present in the retained window AND arriving live is one turn on
  the page's model (the upsert by id); a reconnect's `turns` frame
  replaces the window rather than doubling it; a tool's output
  re-sends the tool's turn whole and lands on the same id; a `turn`
  for a stale `session` or `generation` is dropped; a `turns` frame
  with a new generation clears what was there. These are asserted
  on a pure page-model in Rust (the reducer the page's JS mirrors
  line for line), so the contract is tested where it is stated.
- Projections: the floor for EVERY `WaitingOn` variant, the ad-hoc
  and PR routings, a head correction, a block, and idle — holders
  with verb, object and `since`, the ask, the hue — against
  `in_progress_rows`, `bar_text` and `state_color`; two reviewers
  missing on one commit are two holders, each with its own clock,
  oldest first; an ad-hoc reviewer is a holder with no plan; the
  track is master-first then roster order with every holder marked,
  last acts from the rows; the ledger rows are the TUI's rows in the
  TUI's order with the same ages.
- The mode table: an agent with a transcript offers both, one without
  offers the terminal and the reason.

The browser side — the drawer, the switch, the underline moving with
the turn — is codex's Chrome probe and lloyd's phone.

Mutation-checked, production-only, each asserting the target test
ran: a system-reminder rendered as the person; a sidechain line kept;
a tool output attached to the wrong id; the floor keeping only the
first of two missing reviewers; a stale-generation turn accepted; the
track in roster order with the master not first; a ledger row without
its age.

## Out of scope

- opencode and grok transcripts (their stores are found, not read).
- Answering a block from the page.
- Owning the agent loop (headless harnesses) — a different product.
- Token-level streaming; the files do not carry it.
- Binding beyond localhost, authentication, launching sessions.
