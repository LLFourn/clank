# a-queued-plan-has-a-page

> Fix things about the queue in `clank status --tui`. You should only
> be able to press Enter on a queued plan. From there you should have
> the same interface sort of thing as a normal plan — `o` for open etc.
> You should in addition be able to change priority there, and
> "unqueue" the plan (put it back to drafts). — lloyd

## Today

A queue row in the panel answers three keys directly: Enter opens a
READ-ONLY overlay of the markdown (`OverlayData::QueuedPlan`), `o`
opens its HTML page, and `+` / `-` nudge its priority by 50 in place.
An active plan, by contrast, has a PAGE: `Mode::PlanDetail` with a
button list (open, stash, force finish, squash, purge, back) over the
plan's document, the cursor model the recent plan-page work settled,
and a refresh that rebinds the cursor by action.

So the queue is the odd one out: its actions live on the panel row
as hidden hotkeys, its document is a dead-end overlay, and there is
no way to take an item OUT of the queue without deleting it —
`clank queue remove` unlinks the file, and the body that
`clank queue add` consumed from `.clank/drafts/` is gone with it.

## The model

> A queued plan is a plan that has not started. It is shown on THE
> plan page, in the state "queued", with the actions that state has.

The plan page already does this for its other states: `plan_actions
(st)` gives an active plan stash/force-finish and a finished one
squash, from one `PlanPageState`. Queued is a third state of the same
page — not a second page (lloyd: reuse the plan page's code). So there
is no `QueueDetail` mode, no `QueuePage`, no `queue_detail_nav`, no
`render_queue_detail`: one `Mode::PlanDetail`, one `PlanPage`, one
nav, one renderer, one refresh rebind, and the document-focus model
the last plan settled, all inherited by construction.

- **The panel row does one thing.** Enter opens the page. `o`, `+`
  and `-` leave the row (`OpenQueueHtml`, `NudgeQueue` go); Space
  stays inert on it as now. A stash row is unchanged — not this plan's
  subject; the reviewers can say if it should follow.
- **Opening.** `PanelAction::OpenQueueItem(q)` sets `plan_page` to a
  `PlanPage` whose `stem` is the queue name, whose `st` is the queued
  state, and whose `body` is the queue file's markdown
  (`read_queue_markdown`, which exists), then `Mode::PlanDetail
  { sel: 0 }` — exactly what opening an active plan does.
- **The state is the source.** `PlanPageState` — today three
  booleans, `finished` / `multi_commit` / `repo_paused` — becomes an
  enum of the lifecycle the page opened on:

      PlanStage::Queued { priority }
      PlanStage::Active { repo_paused }
      PlanStage::Finished { multi_commit }

  A queue entry may share a stem with an active or finished plan
  (`queue add` allows it; only `promote` refuses the collision), so
  the page cannot re-derive its source from the stem on refresh —
  preferring the queue would flip an open active page into the queued
  one, preferring plans would break the queue row (codex on 7e12f16).
  So `refetch_plan_page` reads the SOURCE the page opened on:
  `Queued` re-reads `snapshot.queue` for the name — never the plans —
  and closes to the panel when the item is gone (promoted or unqueued
  from a shell); `Active`/`Finished` re-read the plans as today, and
  may move between those two as a plan finishes, never into `Queued`.
  Priority and body are re-read with the facts, and the refresh
  rebind by action identity is already there.
- **The actions** are `plan_actions(st)` for the queued state, in
  order:
  - `o` **open in browser** — `HtmlTarget::Queue(name)` for a queued
    plan, `Plan(stem)` otherwise; the one branch in the open arm.
  - **priority** — a VALUE control, the agent page's shape: the row
    reads `priority ◂ 500 ▸ · lower runs sooner`, and ←/→ (or Space)
    change it in place by 10, PgUp/PgDn by 100, clamped to 0–999.
    `plan_detail_nav` yields `PlanNav::Priority(delta)` on that row
    for those keys; on any other row PgUp/PgDn scroll the document as
    today. Every change goes through `queue::set_priority`, the one
    validated mutation the CLI's `reprioritise` uses; the page's
    state is updated from its return and the cursor stays on the row.
  - `u` **unqueue…** — moves the entry back to `.clank/drafts/<name>.md`
    and removes it from the queue: the inverse of `queue add`.
    Refuses if a draft of that name already exists (nothing is
    overwritten; the message names the file). A NEW
    `queue::unqueue(repo, name) -> PathBuf`, also `clank queue
    unqueue <name>` for CLI parity — the TUI never renames files
    itself. On success the page closes to the panel with a notice
    naming the draft path.
  - `p` **promote…** — `clank queue promote`, behind a confirm, like
    stash: it commits the intro and starts the review cycle. On
    success the page closes; the promoted plan has the active page
    now. Included because the page for a not-started plan would be
    strange without the action that starts it; reviewers may strike.
  - `esc` **back**.
  `plan_hotkey` learns `u` and `p`; `plan_action_row` takes the state
  so the priority row can show its value. The danger styling stays
  purge's alone.
- **Rendering** is `render_plan_detail`, unchanged except: the title
  rule reads `queue · <name>` with the priority as its hint for the
  queued state, and a value row draws its `◂ N ▸` control.
- **An entry's identity is its file.** The parse is lenient —
  `foo.md`, `1-foo.md`, `001-foo.md` all read as one name and even one
  priority — and `scan_queue` keeps every file, so nothing parsed from
  the name can tell two entries apart. `QueueItemView` carries the
  path, the page carries it as `entry`, and the entry is resolved by
  exact path for its body and for its refresh. When that file is gone
  (renamed from a shell while the page was open) the name is followed
  only if it is now unambiguous; two survivors close the page rather
  than guess. Reprioritising from the page goes through
  `set_priority`, the page's identity moves to the path it returns,
  and a refused (ambiguous) name is shown (codex on 2b35cb8, 791a448).
- **The html page is per file too.** `clank html` wrote `queue/<name>
  .html`, so the second of two equal-name files overwrote the first
  and `o` could open the other file's page (codex on fd9b7a2). Pages
  and the index's links are keyed by FILE STEM (`queue/999-foo.html`);
  `--queue <handle>` takes a stem exactly, or a name while that name
  means one file, and refuses an ambiguous name listing the stems; the
  page's `o` passes its own file's stem.
- **The overlay goes.** `OverlayData::QueuedPlan`, its reader and its
  render arm are deleted; the stash overlay stays. The behind-overlay
  refresh carry (`behind_overlay`) is untouched.

## Tests

- Panel: on a queue row Enter yields `OpenQueueItem`; `o`, `+`, `-`
  yield `None`; Space yields `None`. Stash rows keep their `o`.
- `plan_actions` for the queued state is
  `[OpenHtml, Priority, Unqueue, Promote, Back]`; the active and
  finished lists are unchanged.
- `plan_detail_nav`: ←/→/Space on the priority row yield
  `Priority(±10)`, PgUp/PgDn there `Priority(±100)`, and only there —
  on another row PgUp/PgDn scroll the document as today; Enter on
  `open`/`unqueue`/`promote`/`back` yields the action; Enter on the
  priority row yields nothing; `u` and `p` are hotkeys from any row.
- The loop's priority change calls `set_priority` with the clamped
  value (999 stays 999, 0 stays 0) and the page's shown priority
  follows it.
- `queue::unqueue`: the file lands at `.clank/drafts/<name>.md` with
  its body intact and the queue entry is gone; an existing draft of
  that name refuses and moves nothing; an ambiguous name refuses as
  `find_unique` does; `clank queue unqueue` parses.
- Render, through `render_plan_detail` with a queued state: the title
  rule reads `queue · <name>` and carries the priority; the button
  list reads `open in browser / priority ◂ N ▸ / unqueue… / promote… /
  back`; the document body renders beneath. One test — the plan
  page's tests already own the body and the focus model.
- `plan_page_facts` for a queued name yields `Queued { priority }`;
  for a name in neither the queue nor the plans, `None`.
- Same-stem regression: a queue entry AND an active plan both named
  `foo`. Opening the queue row shows the queued actions; a refresh
  keeps it queued with its priority. Opening the active plan shows the
  active actions; a refresh keeps it active. Neither page ever shows
  the other's actions or body. Promoting the queued one from a shell
  closes the queued page (its source is empty) even though an active
  `foo` exists.
- Refresh rebind: priority selected, list unchanged → still priority;
  the item gone → `LogScroll` and no page.
- Duplicates: two queue files sharing a parsed name AND priority
  (`foo.md`, `999-foo.md`) — each row opens its own body, each page
  refreshes onto its own file, a file gone with a duplicate left closes
  its page, and a lone survivor's rename is followed to its new file
  and priority.
- A priority change from the page renames the file and the page's
  identity follows it; a refused change leaves the page as it was.
- `o` from two equal-name rows: two generated pages with their own
  bodies, two targets by stem; a bare name reaches its one file or is
  refused naming the stems.

Mutation-checked with production-only edits: the panel keys restored;
the clamp removed; `unqueue` deleting instead of moving.

## Out of scope

- Editing the queued document in the TUI.
- Reordering by drag or by moving relative to neighbours — the number
  is the priority, and the value control edits the number.
