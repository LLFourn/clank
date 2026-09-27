# web-timeline-navigates-to-pages

## Goal and ordering

Run after `embeddable-web-session-bridge`. Make the web Timeline a navigation surface. Open queued and stashed plans as readable pages in the main content area, with all actions belonging to those pages.

The operator's rule is explicit: **NEVER DISPLAY CONTENT OR ACTIONS WITHIN THE WEB TIMELINE.** This means no expanded plan bodies, Markdown editors, detail panels, management forms, or mutation controls inside Timeline. Timeline entries may show compact names, event summaries, read-only status/priority, and links or navigation buttons to the appropriate page.

## Problem

The queue currently shows a name, editable priority, and edit/promote/unqueue controls. There is no obvious route from the queue entry to a readable plan page. Edit opens a small Markdown textarea inside the ledger. Stash has competing `view` and `HTML` actions: `view` expands source and restore controls inside Timeline. This hides the natural flow of opening a plan, reading it, and acting on it.

Rendered queued and stashed plan pages already exist (`html/queue/<file-stem>.html` and `html/stash/<stem>.html`). The web's `openSitePage`/`plan-view` mechanism already displays generated pages in the main content area. Build on those capabilities and the backend boundary introduced by the preceding extraction plan.

## Intended navigation

1. A queued entry has one obvious Open plan link/button, also reachable by its name. It opens that exact queued plan in the main content area with rendered Markdown as the default view. Reading never requires entering Edit mode.
2. The queued plan page provides Edit, priority change, Promote, and Unqueue. Edit uses the available page area, with a readable preview and Save/Cancel; it preserves drafts and displays save errors on that page. Priority and destructive-action confirmation also belong there.
3. A stashed entry similarly opens one stashed-plan page. Show the complete rendered plan, stash status/dependencies, and commit history there, with Restore and any other existing stash actions on that page. Remove the split `view` versus `HTML` interaction.
4. Queue creation opens a dedicated new-plan page with a full-size editor. Timeline links to that page and contains no creation form.
5. Back navigation returns to Timeline with its scroll position preserved. On phones, pages occupy the main content area outside the Timeline tab's list. On wide screens, the detail page occupies the main area while Timeline remains a navigation sidebar. Opening an item does not grow a detail panel inside that sidebar.

## Work

- Carry the exact queue entry handle/file stem accepted by `queue::resolve_entry` into view data and page navigation; generate the page path through the existing `html::resolve_open_target_path` behavior. Keep stash identity tied to its existing stem lookup. Every mutation must use the existing queue/stash resolver and surface ambiguity or not-found refusals on the owning page. Do not create a second JavaScript identity resolver or derive an action target from a display name. Preserve the distinction between active, queued, and stashed plans sharing a name.
- Add page-level state for the selected resource and its actions, supported by the extracted session/backend API. Reuse the existing validated queue/stash operations and revision checks.
- Reuse the existing generated Markdown pages where appropriate. The current generated-page iframe is sandboxed and static: place authenticated action controls in the surrounding full-page shell or a deliberately designed page component, rather than assuming scripts in the iframe can invoke the application. Preserve the safety of static exports and generated Markdown.
- Remove the inline queue editor, editable priority fields, promote/unqueue buttons, stash source/detail panel, and restore controls from Timeline. Do not retain a second inline path to the same operation.
- Move existing team/agent/session management forms and device actions to dedicated management/settings pages reached from Timeline navigation links. Preserve every existing management action and its result/confirmation on its owning page.
- Add a named `web_timeline_boundary` source gate in `crates/cli/tests/it/` and enforce the rendered rule in `crates/cli/tests/browser/check.js` at phone and desktop widths. Assert generically that `#ledger` contains no `form`, `input`, `textarea`, `select`, `details`, editable content, resource detail panels, or mutation controls; every remaining Timeline button must be marked and exercised as navigation. New controls mounted under `#ledger` must fail the browser gate unless they only navigate. The source gate rejects inline handlers and direct mutation dispatch from the Timeline container so moving a control into a clickable wrapper cannot evade the DOM gate.
- Keep mutation results, validation errors, confirmation state, and unsaved work on their owning page. Unrelated status frames must not close a page or discard a draft. If another surface moves/removes/reprioritizes the item, show its current state or an explicit stale-item response; never retarget the action to a similarly named entry.
- Define navigation after successful promote, unqueue, or restore: show the resulting plan/location where available, or a clear result with a route back to the refreshed list. Do not leave a stale editor ready to resubmit against a disappeared item.
- Install the tested build so the updated UI is available locally.

## Implementation milestones

1. Establish explicit page identity and navigation state using the existing queue/stash resolvers and generated-page routes. Preserve Timeline scroll and page identity across status frames. This should make queued and stashed entries open readable pages before moving their actions.
2. Move queued-plan actions and the full-size new-plan editor to their dedicated pages. Keep revision checks, drafts, errors, confirmations, and post-action navigation on those pages; remove the competing inline queue controls.
3. Move stashed-plan content, commit history, dependencies, and Restore to its page, then remove the inline stash detail path and duplicate `view`/`HTML` actions.
4. Move team, agent, session, and device controls to dedicated management/settings pages. Finish the generic Timeline boundary gate and browser fixtures for narrow and wide layouts, then install the tested build.

Each milestone must leave the UI working and produce a reviewable boundary; the final milestone enforces the operator's rule across the whole Timeline.

## Acceptance

- From Timeline, a person can open a queued plan and read its full rendered body immediately, without clicking Edit. All existing queue actions work from that page.
- A stashed plan opens as a full page with its content, commits, state, and Restore action. Nothing expands inside Timeline.
- Timeline opens a dedicated new-plan page with a full-size editor, and creating a plan there works. The creation form never appears inside Timeline.
- No content editor, resource detail panel, management form, or mutation control is rendered under the Timeline container. Its controls only navigate; this applies to both narrow and wide layouts and to team/device controls as well as queue/stash. A generic browser fixture gate and a source boundary check fail if this rule regresses.
- Team, agent, session, and device management actions remain available on dedicated pages, with their confirmations and results there.
- Browser fixtures exercise opening and reading queued/stashed items, entering and cancelling/saving an edit, changing priority, promotion/unqueue/restore, stale-resource refusal, and return navigation. Use long plan bodies at phone and desktop widths to verify usable reading/editing space and scrolling.
- Browser fixtures include two queued entries sharing a display name: opening and acting on one leaves the other untouched, and an ambiguous reference is refused on the page with both stems identified.
- A status update during an edit preserves the draft, selection, focus, and page. Actions continue to use the existing revision/precondition checks and report failures on the page where they were invoked.
- Generated static plan pages remain readable, and the extracted hosted deployment uses the same page/actions through its session connection.

## Scope

This is a navigation and presentation correction. Preserve existing backend action semantics and confirmation requirements. Keep Timeline's compact chronological entries and navigation; put the actual resource content and operations on dedicated pages.
