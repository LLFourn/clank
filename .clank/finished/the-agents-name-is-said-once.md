# the-agents-name-is-said-once
# The agent's name is said once

## Why

> "look how clean it is. can we make our web view like it. When I look at
> our current site I see the current agent's name mentioned three times:
> Once at the top right, then once in the tab then once below that. Can we
> instead just have the current agent mentioned once and have a dropdown
> when you press it to choose another. Keep all the space for clean
> display."

Said with a photograph of Claude's own app: a title, one small line under
it, a single `···` at the right, and everything below that is the
conversation.

And, on the picker's default:

> "I think the default selection should be 'auto' so it's on whoever's
> turn its on. So if there's two agents the dropdown has three values
> (claude, codex, auto) with auto being the default. Please clean up the
> data model if that's what's causing the duplication."

Ours says `claude` three times before it says anything claude did:

1. `.floor` — the turn line, 22px Martian Mono.
2. `.track` — the tab, 16px Martian Mono.
3. `.head` — the view's own label, 15px.

Four rows of chrome — `top`, `floor`, `track`, `head` — stack above the
content, on the surface with the least room for it.

## The model

**One agent, one entry. What an agent owes belongs to the agent.**

The server hands the page two lists of agents and asks it to join them:

- `Floor.holders: Vec<Holder>` — label, verb, object, since — only the
  agents the gate is waiting on.
- `track: Vec<Position>` — label, role, holds, last — every agent.

`holds` IS that join, computed on the server as
`floor.holders.iter().any(|h| h.label == a.label)`. An agent's obligation
is sayable only where the holders list is painted, so any part of the
page that wants to say what an agent is doing must print the name again
to say it — which is precisely the three-times complaint. The same split
duplicates `hue` (`WebFacts.hue` and `Floor.hue` are assigned from one
value) and makes `Floor.ask` a copy of `ledger.blocks[0]`.

Collapse the two lists into one:

```rust
struct Owed { verb: String, object: String, since: Option<i64> }
struct Position { label, role, owes: Option<Owed>, last, transcript }
struct WebFacts { lamp, plan, hue, correction, agents: Vec<Position>, ledger }
```

`Floor` is deleted. `holds` becomes `owes.is_some()`, so an agent that
holds the turn but has nothing to owe is unrepresentable rather than
merely unlikely. The floor's two remaining facts were never about an
agent: `correction` (HEAD's tag needs fixing) moves up to `WebFacts`, and
`ask` was already in `ledger.blocks`.

With one entry per agent, the header names the agent once and reads
everything else out of that agent's entry.

And with the obligation on the entry, **who is up is derived, never
stored.** `holds` was a stored answer to that question and it is gone;
the header's `auto` selection asks `owes` afresh every time the facts
arrive.

## Deliverables

1. **The header is two lines and nothing else.**

   ```
   claude ▾ auto                                        ···
   ● working the-agents-name-is-said-once   4m    transcript · terminal
   ```

   Line one: the shown agent, Martian Mono 700, ~22px, with a caret; the
   name and caret are one button. `···` at the right opens the ledger —
   the `≡` we have, in the reference's glyph, keeping its 44px target.

   Line two, small and muted: what that agent owes, said the way the
   floor says it — breathing dot, verb, object, clock — read from `owes`.
   An agent that owes nothing shows its `last` act instead, with no dot.
   The text truncates; the clock does not.

2. **The picker replaces the track, and its first row is `auto`.**
   Pressing the name opens a panel under the header, full width on a
   phone: `auto` first, then master, then the roster's order. Each agent
   row carries the same clause line two would give it, and a row whose
   agent owes something breathes. The current selection is marked.
   Tapping selects and closes; Escape, a tap outside, and re-pressing the
   name close it too. Rows are buttons, focusable, in a stable order — a
   list you reach for with a thumb must not move under it.

   ```
   auto      follows the turn — claude now
   claude    master   ● working the-agents-name-is-said-once   4m
   codex     final    ● reviewing 52cf9f4                      2m
   kimi      commit   ✓ continued, 12m ago
   ```

3. **Selection is a mode, not a name.**

   ```js
   selection = { auto: true } | { label: 'codex' }
   ```

   Auto is the default, and it resolves to the agent with the oldest
   outstanding obligation, else master. The viewer's choice persists in
   `localStorage` beside the per-agent view modes, so a pin survives a
   reload and a phone that comes back from sleep.

4. **The floor's oldest-first rule moves to the page, and the server
   stops sorting.** Auto asks one question — who has been kept waiting
   longest — and answers it from the `since` on each entry, an undated
   obligation last, ties keeping the roster's order. The server's own
   sort existed only to break ties in a list that could name an agent
   twice; one entry per agent cannot, so it goes rather than standing
   as defence against a state no routing produces.

5. **A draft owns the view, and there is ONE place the view moves by
   itself.** Following the turn means the shown agent can change on its
   own, and the say box sends to whoever is shown: a move under a draft
   delivers the message to the wrong agent. So `retarget()` is the only
   automatic transition and it returns early while the box has text —
   including when the agent the draft was typed to leaves the roster,
   where the honest answer is a header that says so and a send button
   that goes quiet with the pane, not a new recipient. A send clears the
   box without firing an `input` event, so releasing the draft says so
   itself. The small muted `auto` after the caret explains a title that
   changes by itself.

   (codex on ae4e5c2 found both halves of this: the roster-validity
   branch bypassed the guard, and a successful send never re-resolved.
   Two automatic paths, one of them draft-aware, is the shape of the
   bug — hence one.)

6. **When pinned, the caret says the turn is elsewhere.** If an agent
   other than the shown one owes something, the caret carries a breathing
   dot. In auto the shown agent IS the turn, so the dot cannot appear;
   pinned, it is what survives of the floor's "whose turn is it" when one
   name is on screen.

7. **A block is an interruption, so it gets a strip only while it
   exists**: a full-width line under the header carrying the question and
   how to answer it, gone when there is no block. `correction` gets the
   same treatment, naming master. Neither is permanent chrome.

8. **The feed's down state replaces the state line.** `connecting` /
   `reconnecting` currently live in a corner as a permanent word. While
   the socket is down, line two says so instead of saying what the agent
   is doing — because what it would say is stale. `#live` as a fixture
   goes away.

9. **Deleted**: `.floor`, `.track`, `.head`, `#live`, `#why`, and the
   ledger's copy of the blocks — the strip says them now, and saying
   them twice is the fault this plan is about. `#session` stops being a
   row of its own and becomes the ledger's heading, which the desktop
   column has never had.
   `document.title = window.CLANK_SESSION || 'clank'` STAYS, verbatim —
   `web/mod.rs` string-replaces that exact expression to inject the
   session name, and an edit that reflows it fails silently and serves
   every session a page titled `clank`. The expression becomes a named
   constant behind `page_for`, so the anchor is testable.

## Tests

- `web_facts` gives one list: both missing reviewers carry `owes` with
  the verb, the sha they owe and their clock; master's `owes` is `None`;
  master first, then roster order; `correction` is on `WebFacts`. The
  existing `the_web_facts_are_the_tuis_own_derivation` becomes this test
  rather than gaining a second one.
- No `Floor`, `Holder`, `holders` or `floor` remains in the web-facts
  model — scanned in the file that derives it and the module that
  consumes it, not across the crate: `status_tui/lease.rs` has an
  unrelated `Holder` that stays (codex on 52cf9f4).
- Each `owes` carries its `since`, so auto has an oldest to pick, and a
  broken HEAD tag reaches the page as a sentence naming the commit to
  amend and the plan its tag missed.
- Every `facts.…` path the page walks is a field the serialized facts
  have, and an agent entry's shape is pinned by name. In JavaScript a
  renamed field is a blank space, not an error; this is the only thing
  that would notice.
- The served page carries the session as JSON and no longer carries the
  literal `window.CLANK_SESSION || 'clank'` — that substitution has no
  test today, which is exactly the silent-anchor hazard deliverable 9
  names.
- Every element id the page's script asks for with `$('…')` exists in the
  markup, and every `id="…"` in the markup is asked for. A structural
  test the redesign can be checked against: a renamed id or an orphaned
  node fails it.
- **A browser harness, run by hand**: `crates/cli/tests/browser/` drives
  the real `page.html` in chromium against synthetic frames — the draft
  invariant above, auto re-resolving after a send, the clock at three
  widths, the picker's marking, a pin surviving a reload, Escape, and a
  browser that refuses site data. It is NOT wired into `cargo test`:
  that would put node and a chromium download between the repo and its
  tests. Each check is mutation-verified — reverting the fix it guards
  makes it fail.
- The page must survive a browser that refuses storage. Its first
  statement read `localStorage` unguarded, so a private window got a
  blank page; found by the harness, not by reading.
- Mutation-check each claim.

## Out of scope

- The transcript and terminal views themselves, and the say box.
- What the ledger contains; only its opener changes.
- The desktop two-column layout stays as it is. The picker works there
  too — the header is the same header at every width.
