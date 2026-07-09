# tui-confirmations-own-pages

## Problem

`clank status --tui` has two different confirmation UX patterns:

- Plan-page actions already render dedicated confirmation pages
  (`render_plan_confirm` via `Mode::Confirm` when `action.is_plan_page()`).
- Roster add/remove confirmations render as an inline prompt appended under the
  main status/agents view. After accepting/canceling they jump back to the
  agents panel, usually on the `+ add agent` row.

The inline main-screen prompt is noisy and easy to miss. It is especially
annoying for add/remove agent flows because the user navigates into a picker or
detail page, triggers a confirmation, then visually gets dumped back into the
main status layout with a prompt instead of a clear confirmation page.

## Goal

Make TUI confirmations consistently page-shaped. Every `Mode::Confirm` should
render through a dedicated confirmation page, never as inline rows under the
main status/agents view. Any confirmation that would currently show as a prompt
on the main status view should either be removed when unnecessary or replaced
with a dedicated confirmation page with readable choices and consequences.

## Scope

1. Audit every `ConfirmAction` rendering path in `crates/cli/src/cli/status_tui`
   and identify any confirmation that is not its own page.
2. Replace roster add/remove's inline prompt in `render_at` with a dedicated
   roster confirmation page, analogous to `render_plan_confirm`.
3. Consolidate confirmation rendering so there is no split path:
   - remove the inline confirm arm in the main `render_at` agents-section
     renderer, including the now-dead plan-page `ConfirmAction` branches inside
     it
   - route roster add/remove and plan confirms through page renderers before
     the main status layout is built
   - drop the `action.is_plan_page()` guard from the render dispatch once all
     confirms have page renderers
4. Keep `ConfirmAction::is_plan_page()` only for navigation/return-target
   decisions. It should no longer decide whether a confirm gets page-shaped
   rendering.
5. The roster confirmation page should clearly show:
   - the operation (`add` or `remove`)
   - the target agent label/tool when available
   - the consequence: edits local `.clank/config.json`
   - readable options and default (`[Y]es/[N]o`, Enter behavior)
6. Preserve safe defaults:
   - add can keep Enter-as-yes if that remains intentional
   - remove must default to no
   - `q`/Esc cancel, not quit
7. After confirm/cancel/error, return to a sensible place without a jarring
   main-screen prompt transition:
   - add/cancel should return to the add picker or the `+ add agent` row as
     appropriate
   - remove/cancel should return to the agent detail/panel context where
     possible
   - successful mutation may refresh and rebind normally
8. Update refresh-mode rebinding so an in-progress roster confirmation does not
   silently collapse to the main panel unless its target is no longer valid.

## Non-Goals

- Do not redesign the agent picker.
- Do not change the underlying add/remove commands or roster persistence model.
- Do not change plan-page purge/stash confirmations except as needed to share
  rendering helpers.

## Validation

- Add/adjust TUI render tests proving roster add/remove confirmations render as
  standalone pages, not as inline rows under the agents section.
- Add/adjust tests proving the main status/agents renderer has no inline
  confirm output path and every `Mode::Confirm` renders a page.
- Add/adjust input/mode tests for defaults and cancel behavior.
- Add/adjust refresh/rebind tests for roster confirm mode.
- Run focused `status_tui` tests.
