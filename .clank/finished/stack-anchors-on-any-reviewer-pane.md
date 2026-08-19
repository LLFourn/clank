# stack-anchors-on-any-reviewer-pane

## Problem

Adding a reviewer opens its pane in the wrong place. Reported live:
adding `kimi` to `recovery-scan` (roster claude/master, kimi/commit,
ruthless/final) opened the pane BELOW the status pane instead of
joining `ruthless`'s reviewer stack.

A clank-bound `kimi` pane was found stranded in a foreign tab of
session `clank-fsctl` and closed by hand. Both it and the working
reviewer had cwd `/Users/llfourn/src/clank` and resumed the SAME
opencode session id recorded in `.clank/agents/kimi/config.json`; only
the working one (pane `terminal_201`, in the `clank` tab) had a `clank
wait` attached. So the same agent got two panes, and one of them
opened in a tab it had no business being in.

Any anchor search must match the title the TUI produces, not the one
`agent_pane_title` composes: the status TUI stamps a status glyph onto
every agent pane it classifies (`status_tui/zellij.rs:278`), so live
titles read `👀 codex (reviewer)` / `💤 ruthless (reviewer)`. Matching
the bare form alone would miss every classified pane.

Placement is left wrong because repair is now bounded
(`placement-reads-zellij-stacks-correctly`) — correctly, since
unbounded repair was the focus-churn bug. A bound turns "eventually
maybe right" into "wrong and left alone", so the first placement is
effectively the only one.

Nothing moves a pane between tabs, either: zellij exposes
`BreakPaneLeft` / `BreakPaneRight` / `BreakPane` as KEYBINDINGS (Tab
mode `[` / `]` / `b`) and no `zellij action` subcommand in 0.45.0
crosses tabs (`move-pane` rotates within a tab). So a pane opened in
the wrong tab can only be closed and respawned — which is why the
tab scoping below is a correctness requirement, not hygiene.

## Why it lands wrong

`add_reviewer_pane` (`open_zellij.rs:1126`) does not choose the
position at all:

- It focuses an anchor pane only to decide the TAB — its own comment
  says so: "Focus decides which TAB `new-pane` opens in — nothing
  more."
- `new-pane` with no direction "will try to use the biggest available
  space", which is why a fresh pane lands under the status pane.
- The correction is the `stack` call afterwards, which collects ids by
  EXACT launch command (`agent_pane_label`).

So both the anchor lookup and the stack membership depend on exact
`clank agent start <label> --repo <path>` matches. Any pane that does
not match exactly — a differently-spelled repo path, a pane launched
before a rename, a manually restarted agent — is invisible to both.
When the anchor lookup finds nothing, focus is never moved, so
`new-pane` opens in whatever tab happened to be active. The same miss
that loses the position loses the TAB, which is how a clank-bound pane
ends up in another repo's tab.

## Approach

1. **Anchor stacking on any reviewer pane in the caller's tab**, found
   by title rather than by exact command. If a reviewer pane is
   present, the new pane stacks with it.

2. **Match the title zellij actually reports, not the one we compose.**
   `agent_pane_title` emits `{label} (reviewer)`, but the status TUI
   retitles agent panes to `{emoji} {label} (reviewer)`
   (`status_tui/zellij.rs:278`), and `ZellijPane::title`'s own doc
   records the prefix as present. Equality against `agent_pane_title`
   would therefore match only never-retitled panes, missing every pane
   the TUI has stamped — the search would silently under-find while
   looking correct.

   The rule is: strip the leading glyph, then match the role suffix.
   `strip_leading_emoji` is already `pub(crate)`
   (`status_tui/text.rs:320`) and already called from `open_zellij.rs:452`,
   so no new plumbing. `parse_agent_panes`
   (`status_tui/zellij.rs:129`) implements exactly this shape today —
   strip, then `strip_suffix(" (reviewer)")` — and is the precedent to
   follow.

3. **Name the tab referent explicitly: the caller pane's tab.** The
   anchor focus is what DECIDES which tab `new-pane` opens in, so the
   search cannot discover the tab from its own results — it must be
   scoped to a tab known beforehand. Use the caller pane's `tab_id`,
   exactly as `compose_promote_layout` does
   (`open_zellij.rs:1403-1407`), and skip when there is no caller
   pane ref.

   This scoping is load-bearing, not hygiene: `list-panes` spans ALL
   tabs — the live session above has 13 — and `(reviewer)` appears in
   nearly every one. An unscoped search would anchor onto another
   repo's reviewer, putting the pane in a tab nothing can move it out
   of.

4. **This does NOT re-open the rejected design, and the plan says why.**
   `zellij-pane-placement-and-cost` rejected titles for IDENTIFYING
   agents: a title "carries no ownership marker" and "cannot round-trip
   the legal label domain", so classification by title fails OPEN — a
   wrong label that never converges. That reasoning is about deciding
   WHO a pane is, which drives roster decisions.

   Anchoring asks WHERE to put a pane. A wrong anchor costs one
   misplaced pane; it never feeds a roster decision. Classification
   stays on exact command matching, unchanged.

5. **Consider opening adjacent rather than "biggest space".**
   `new-pane` accepts a direction; focusing the anchor and opening
   next to it would land the pane close to correct before any repair
   runs. Decide with evidence, since a wrong direction is its own
   misplacement.

## Implementation note

`stack_reviewer_panes` collects by exact command, so the title-found
anchor's id must be added to the set it stacks. Once it is, the
exact-match read-back in `reviewers_are_stacked` sees the new pane as
a LONE reviewer and converges provided it is not stacked with the
status pane — so a title-stacked end state reads as converged. Leave a
comment at the call site recording that this blind spot is known and
deliberate, so a later reader does not "fix" it into churn.

## Required tests

In-process, on captured `list-panes` shapes (no live zellij):

- A new reviewer stacks with an existing reviewer in the caller's tab,
  found by title, when NO pane matches the exact launch command.
- An emoji-prefixed `👀 codex (reviewer)` IS chosen — the shape the
  TUI's retitle produces for every pane it classifies.
- A bare `x (reviewer)` is chosen too: the glyph strip is a no-op on
  it, and a pane the TUI has not classified keeps that form.
- An emoji-prefixed master title is NOT chosen.
- A reviewer pane in a DIFFERENT tab is never chosen as the anchor.
- The status pane is never chosen.
- With no reviewer pane in the caller's tab, behaviour is unchanged
  from today (no anchor, no crash); likewise with no caller pane ref.
- Exact-command matching still governs CLASSIFICATION: a pane titled
  `x (reviewer)` from another repo does not become a roster member.

## Acceptance

- Adding a reviewer to a tab that already has one stacks with it.
- The anchor search matches the retitled shape the TUI produces.
- No title-derived value ever reaches a roster or convergence
  decision.

## Out of scope

- The bounded repair itself. It is correct that repair stops; this
  plan reduces how often it is needed.
- Recovering panes already stranded in a foreign tab — not scriptable
  in zellij 0.45.0; close-and-respawn is the remedy.
- Duplicate panes resuming one agent session (observed: two processes
  on `ses_…so`, both cwd the clank repo, only one with a `clank wait`).
  Cause NOT established — do not guess at one here. Note only that
  `add_reviewer_pane`'s idempotence guard is
  `find_pane_by_command(..).is_some()`, so whatever makes a live pane
  unnameable by command would also make it read as absent.
- The `default`-team fallback that seeded these extra reviewers.
