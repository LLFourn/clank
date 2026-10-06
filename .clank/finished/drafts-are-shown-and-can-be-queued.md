# drafts-are-shown-and-can-be-queued
# Drafts are shown and can be queued

## Why

The status TUI shows the plan queue but not `.clank/drafts/`. So a person
cannot see how many plan ideas are waiting there, read one, or move one
forward without the command line.

Moving a queued plan back to drafts already exists. The queued plan page
has Unqueue (`u`), backed by `clank queue unqueue`. Only the drafts side is
missing.

## What to build

- **One collapsed row.** Below the queue in the status view, show a single
  row, `drafts: N`, when `.clank/drafts/` holds any drafts. N counts the
  `.md` files `clank queue add` would accept. With none, show nothing.
- **A drafts page.** The row can be selected like any other. Enter opens a
  page listing each draft:
  - its name;
  - its title, the first heading line;
  - its modification time, newest first.

  Selecting a draft opens a read-only view of its body, as the queued plan
  page does.
- **Moving a draft forward.** From a draft's page:
  - **Queue (`e`; `q` is the TUI's quit)** runs the same core as `clank queue add <name>`. The
    draft is consumed into `.clank/queue/` at the default priority.
  - **Promote (`p`)** queues it and immediately promotes it to the active
    plan, as `clank queue promote` would. It sits behind the same confirm
    and gate as promoting from the queued plan page, and is refused, with
    the reason, when a plan is already active.
  - **Back** returns to the drafts list.
- **Same validation as the command line.** Both actions call the library
  cores the commands use, so an empty or header-only draft is refused with
  the command's own message, and nothing is half-moved.
- **Demotion stays where it is, and shows up here.** Unqueue on a queued
  plan's page already moves it to drafts. Its draft appears in the drafts
  row and page at once.
- **Kept live without polling.** The count and list are derived from
  `.clank/drafts/` on the TUI's existing refresh of `.clank/`. That
  directory is already watched, so this adds no polling.
- **The remote view gets the row.** Where the status projection is served
  to the web or remote view, the drafts count is included, so the row reads
  the same everywhere. Acting on drafts remotely is out of scope.

## Acceptance

- **The row:**
  - With two drafts, the status view shows one `drafts: 2` row below the
    queue.
  - With none, no row.
- **The page:** Enter on the row lists both drafts, newest first, with
  their titles, and Enter on one shows its body.
- **Acting on a draft:**
  - Queue moves the draft into the queue. It leaves `.clank/drafts/` and
    shows in the queue at the default priority.
  - Promote moves it into the queue and promotes it to the active plan, and
    is refused with the reason while a plan is active.
  - A header-only draft is refused by both actions with the command's
    message, and stays in drafts.
- **Round trip:** Unqueue on a queued plan's page makes it appear in the
  drafts row and page without restarting the TUI.
- **Commands unchanged:** `clank queue add`, `clank queue unqueue` and
  `clank queue promote` keep their existing behaviour.

## As built

- **One enumeration.** `queue::scan_drafts` gives every `.md` in
  `.clank/drafts/` whose stem is a plan name, newest first, then by name.
  - The count, the panel row, the list, the remote count and the draft
    page all read it through `StatusSnapshot.drafts`.
  - Eligibility is the file name only, so a header-only draft is listed
    and readable; queueing it is what refuses it.
- **Cores.**
  - `queue::queue_draft` is `add` at `ADD_PRIORITY`, the constant the
    `queue add` default now names.
  - `queue::promote_draft` holds the plan-mutation lock and refuses an
    active plan or a taken plan name before `add` consumes the draft.
    `add` itself refuses a bad body or a queued name before consuming.
    Then it runs the same promotion `promote_entry` does, which was split
    into lock-and-check plus `promote_admitted`.
  - A failure names where the body is at that moment. Before the move into
    plans/, that is the queue file it was given. After the move (a refused
    stage or intro commit), it is `plans/<name>.md`, with the note that the
    intro was not committed; this also covers `queue promote`.
- **TUI.**
  - `PanelRow::Drafts` sits under the queue.
  - `Mode::DraftsList` is the list.
  - A draft's page is the plan page in `PlanPageState::Draft`. Its source
    is its own file; its actions are Queue (`e`, since `q` is the global
    quit) and Promote (`p`), both behind the existing confirm. Its Back
    returns to the drafts list row it came from.
  - After a queue or promote, the page follows its plan into the queue or
    the plans.
  - The list keeps the selected draft in view. Its window is derived from
    the cursor at each render, so it fits any pane height and a resize.
  - The cursor follows the draft it was on across a refresh, by file,
    since an insertion or an edit reorders the list
    (`input::rebind_draft_sel`). It falls back to the same row, or the
    last, only when that draft is gone.
- **Live refresh.** `repo_watch::CLANK_STATUS_ONLY_DIRS` (`drafts`) is
  status presentation only:
  - the status diff watcher's filter lets it through;
  - the reuse fingerprint covers it;
  - the gate watcher still ignores it, so a draft edit wakes no
    `clank wait` or monitor.
- **Remote.** `Ledger.drafts` is the count. `page.html` draws one
  read-only `drafts: N` row under the queue list, or none at zero.
