# queue-visible-in-html-and-tui

Bring the clank queue to life: surface queued plans in both the HTML report and
`clank status --tui`, and let the TUI reprioritise and open them. Today the
queue is invisible unless you run `clank queue` on the CLI or read
`.clank/queue/` by hand. Low-priority polish — no behaviour change to the queue
itself, only new read/inspect surfaces (+ a TUI reprioritise action).

## Background (existing machinery to reuse)

- Queue storage: `.clank/queue/<NNN>-<name>.md`, priority 0-999 encoded as the
  3-digit filename prefix (`queue.rs`: `QueueEntry { priority, name, .. }`,
  `scan_queue` at :174 parses `stem[..3]`, `queue_dir` at :126). Lower NNN =
  higher priority. Changing priority = renaming the file's prefix.
- HTML: `html.rs::render_index` (:382) builds `index.html`; `build_site` (:280)
  also writes per-plan / per-commit pages. The TUI already opens pages via the
  `html_open_argv` builder + the `o` overlay key (tui-open-in-html).
- TUI: `status_tui` renders a top panel and a bottom oneline LOG; overlays open
  on Enter (`OverlayData::Plan`/`Commit`) with `o` = open-in-browser.

## Part 1 — queued plans in the HTML report

- `build_site` reads `scan_queue(repo)` and renders one page per queued plan,
  e.g. `.clank/html/queue/<name>.html` (mirror `render_plan_page`: the plan body
  as markdown, plus a header showing name + priority).
- `render_index` grows a QUEUE block at the TOP of the homepage (above the
  commit/plan history) listing queued plans ordered by priority (NNN asc), each
  linking to its `queue/<name>.html` page and showing its priority. Empty queue
  → omit the block (no empty header).
- Add a `--queue <name>` open target to `clank html open` (mirror the `--commit`
  target added in tui-open-in-html), resolving to `queue/<name>.html`, so the
  TUI and CLI can open a queued plan page.

## Part 2 — QUEUE section in `clank status --tui`

A new section BETWEEN the agents panel and the LOG:

- Render a `QUEUE` header + one row per queued plan (name + priority),
  ordered by priority. Reuse the panel's existing row/selection style; keep it
  compact (it sits above the log, which must stay usable).
- Navigation: the existing up/down selection extends into the QUEUE rows.
  - **Enter** on a queue row → open a read overlay of the plan body (reuse the
    plan detail overlay used for `OverlayData::Plan`; the source is the queue
    file, not `.clank/plans/`).
  - **o** on a queue row / its overlay → open its HTML page (`queue/<name>.html`)
    via the same detached `html open --queue <name>` spawn the plan/commit
    overlays use.
  - **Reprioritise**: a key (propose `+` / `-` to nudge priority by a step, or
    `[` / `]`; reviewer's call) that renames the queue file's NNN prefix and
    triggers a refresh. This is the only MUTATING action — see below.

## The reprioritise primitive (git-layer-style discipline)

Do NOT rename queue files inline in the TUI. Add ONE queue primitive —
`queue::set_priority(repo, name, new_priority)` — that both the TUI action and a
new `clank queue reprioritise <name> <priority>` CLI subcommand call, so there is
a single validated implementation (0-999 range, dup/rename handling, reuses the
`{:03}-{name}.md` naming already in queue.rs). The TUI must go through it, not
`std::fs::rename` by hand.

## Decisions to flag for review

1. Reprioritise keybinding + step (nudge by 50? jump to a typed value?). Lean:
   `+`/`-` nudge by 50, clamped 0-999, since typing a number in the TUI is
   heavier. Reviewer's call.
2. Queue-page styling: full plan render (like a plan page) vs a lighter card.
   Lean: reuse `render_plan_page` so queued and active plans look consistent.
3. TUI QUEUE section when the queue is empty: hide the section entirely (no
   empty header), matching the HTML behaviour.

## Tests

- `render_index` includes a QUEUE block (with the queued names + priorities,
  priority-ordered) when the queue is non-empty, and omits it when empty.
- `html open --queue <name>` resolves to `queue/<name>.html`; a non-existent
  queued name errors clearly.
- `queue::set_priority` renames `<old-NNN>-<name>.md` to `<new-NNN>-<name>.md`,
  validates the 0-999 range, and is a no-op-safe / idempotent rename; the CLI
  subcommand round-trips.
- status_tui: the pure row producer emits QUEUE rows between the agents panel and
  the log, priority-ordered; selection can land on a queue row; `doc_nav` routes
  Enter→read-overlay and o→open-html for a queue row (unit test on the pure
  layer, per the no-binary-spawn rule).

## Acceptance

- `clank html` renders a queue page per queued plan and a priority-ordered QUEUE
  block at the top of the homepage (omitted when empty).
- `clank status --tui` shows a QUEUE section between AGENTS and LOG; Enter reads a
  queued plan, o opens its HTML page, and the reprioritise key changes its
  priority through the shared `queue::set_priority` primitive.
- `clank queue reprioritise <name> <priority>` exists and shares that primitive.
- clippy at baseline; tests pass.

## Deploy

`cargo install --path crates/cli --force` (no skill change; regenerate HTML on
next `clank html` / open).
