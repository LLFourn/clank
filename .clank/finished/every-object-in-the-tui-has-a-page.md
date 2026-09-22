# every-object-in-the-tui-has-a-page
# Every object in the TUI has a page

## Why

> "it seems we still have an exception to the rule that going to an
> object in clank tui does not have weird hotkeys in a top menu bar but
> has a proper menu in the page. Can you find it."
>
> "fix those pages. Also on a commit page we need a button to open the
> original plan."

Agents, plans (queued, active, finished), GitHub events, waits and the
remote each open a PAGE: a menu of rows, each naming its key, over the
content (one-menu-not-five). Two objects still open a bare document
whose only actions are hotkeys advertised in the top rule
(`↑↓ scroll · o browser · Esc back`):

- **a commit** — Enter on a commit or review row in the log opens the
  commit overlay (`OverlayData::Commit`, `build_commit_lines`);
- **a stashed plan** — Enter on a stash row opens `OverlayData::StashedPlan`
  (`render_plan_doc`), which cannot even restore the plan.

A commit also cannot lead to the plan it belongs to.

## The model

**An object is a page: a menu over its document.** The plan page is
the shape — buttons first, the document scrolling beneath
(tui-plan-page-redesign). A commit gets a page of its own on that
shape, and a stashed plan becomes the plan page in one more state.
The two overlays, their key-hint rules and their document key routing
go; the overlay mechanism stays for what is a document rather than an
object (an agent's Info, a failure, a link).

**A commit's plan is the commit's own tag.** The plan a commit belongs
to is the `[plan]` tag in its subject, the same attribution the fold
uses — not the umbrella the log happens to draw it under, which
windowing and GitHub rows can split. A commit tagged with several
plans has a row for each; an ad-hoc commit has none; a tag naming a
plan with no page (purged) gives no row. Opening one is the existing
plan-page path (`plan_page_facts`), and Back from a plan page reached
this way returns to the commit page it came from.

## Deliverables

1. **The commit page.** `Mode::CommitPage` with the commit held in the
   loop, as the plan page holds its plan. Rows: `o open in browser`, one
   `open plan <stem>` row per plan the commit's tag names that has a
   page, and `esc back`. Beneath them the document — message, each
   review, the file stats — scrolls as the plan page's does. Enter on a
   review row opens the page scrolled to that reviewer's feedback, as
   the overlay does today. The page's rule carries no key hints.
2. **Open plan.** The row opens that plan's page; Back there returns to
   the commit page, at the scroll it had.
3. **The stash page.** A stash row opens the plan page in a `Stashed`
   state: `o open in browser`, `restore` (`clank stash pop`'s core,
   behind a confirm as stash is), `esc back`, over the stashed plan's
   markdown. Once restored, the next refresh shows the plan as active.
4. **The overlays go.** `OverlayData::Commit`, `OverlayData::StashedPlan`
   and the document key routing only they used are removed; Info,
   failure and link documents keep theirs.

## Tests

- A tagged commit's page rows are open in browser, open plan <stem>,
  back; an ad-hoc commit's are open in browser, back; a commit tagged
  with two plans has a row per plan; a tag naming a purged plan gives
  none.
- Open plan opens that plan's page; Back returns to the commit page at
  its scroll.
- Enter on a review row opens the commit page scrolled to that
  reviewer's feedback.
- A stash row opens the plan page Stashed with its three rows, over the
  stash's markdown; confirming restore pops it, and a refresh shows it
  active.
- Neither page's rule advertises a key (no `o browser` in the top rule).

Run `cargo test -p clank --lib cli::status_tui`, then the whole
`-p clank --lib` suite.

## Out of scope

- The Info, failure and link documents — documents, not objects.
- `clank log` and the web page.
