# tui-agent-detail-page
# status --tui: per-agent detail/config screen

Builds on the shipped agents panel. Today an agent row only toggles
auto (SPC) and DEL removes a reviewer from the main panel. Make Enter
on an agent open a full-screen DETAIL page that shows the agent and is
the one place you manage it — tier, master, auto, removal — and
surface the reviewer TIER on the main panel so the kinds are
distinguishable.

## Why / what the user asked for

- The main panel collapses every reviewer to "reviewer"; you can't see
  commit-tier vs gate-tier. Show the tier.
- Pressing Enter on an agent should open a config page showing its
  details (invocation command, description, tier, auto) and let you:
  toggle auto, switch tier (commit↔gate), promote to master, and
  remove it.
- Remove moves OFF the main panel — DEL there goes away; removal
  happens on the detail page (after Enter), behind the confirm.

## Architecture: a new Mode on the existing machine

The agents panel is a `Mode` state machine (LogScroll / AgentPanel /
AddPicker / Confirm). Add one variant — `AgentDetail { idx }` — for the
full-screen detail/config view, the same way AddPicker was added. Enter
on an agent row routes to it (pure, via `agent_panel_action`); Esc
returns to the panel. The detail page is a small ACTION MENU (reusing
the cursor + full-row selection band already built), not a pile of new
letter-keybindings:

```
ConfirmAction stays the remove path; the detail page's actions are a
selectable list, ⏎ activates the highlighted one, ↑↓ move, Esc back.
```

Keep ALL routing pure + tested (the discipline that carried M1–M3):
extend `agent_panel_action` with `OpenDetail(idx)`, and give the detail
page its own pure key→action function (cursor move / activate / back).

## Reuse the existing cores (no new roster-write logic)

`crates/cli/src/cli/agent.rs` already has pub, env-free, tested cores —
the detail page is a front-end to them, never a reimplemented write:
- promote to master: `set_repo_master(repo, &label)` — also DEMOTES the
  current master (surface what tier it lands in; the core decides).
- switch tier: `set_repo_review(repo, &label, ReviewKind::Commit|Gate)`
  (refuses master — so "switch tier" is hidden/disabled on the master).
- remove: `remove_repo_agent(repo, &label)`.
- auto: `agent_store::set_auto_mode(repo, &label, mode)` (already wired).
All edit the TRACKED `.clank/config.json` (except auto, which is the
gitignored per-agent file), so role/tier/remove dirty the tree — the
committed-config weight the confirm names; same distinction as today.

## Snapshot: carry the tier

`AgentAutoRow` currently holds `role: vocab::Role` (Master/Reviewer).
Replace it with the roster tier `RosterRole` (Master/Commit/Gate) — the
snapshot already builds `agents` from the SEPARATE commit/gate reviewer
sets, so the tier is known at build time with no extra reads. The main
panel then renders `master` / `commit` / `gate` per agent (distinguish
the kinds); the detail page reads the same.

## The detail page (frontend-design)

Full screen (early-return in `render_at` like AddPicker), hard-clamped
to `rows` with single-line fields (reuse `one_line` so a multiline
`initial_prompt` can't overflow). Layout:

```
 AGENT · codex ───────────────────────────────
   tool         codex
   tier         commit reviewer
   auto         ▶ on
   invocation   codex --profile deep
   purpose      <initial_prompt, one line, or "—">

 ── actions ───────────────────────────────────
 ▸ toggle auto (→ off)
   switch tier → gate
   promote to master
   remove from team
   ← back

 ↑↓ move · ⏎ select · Esc back
```

- The selected action uses the unified selection band (one selection
  style, as on the panel/picker).
- `purpose` is the `initial_prompt` (the only "what is this for" clank
  has — name the gap as before; a real purpose field stays future).
- Master's page omits "switch tier" and "remove" and "promote to
  master" (already master); it keeps auto + back. (Changing the master
  is "promote a DIFFERENT agent", per the core.)
- "remove from team" → the existing `Confirm` modal (committed-config
  consequence, default No).

## Main-panel changes

- Show the tier (`master`/`commit`/`gate`) per row instead of the
  flat `reviewer`.
- Enter on an agent row → `AgentDetail`; Enter on "+ add" → picker
  (unchanged); SPC still toggles auto inline (quick path kept).
- DEL is removed from the panel (no `RequestRemove`/`MasterNotice`
  there); removal lives on the detail page. Drop the master-notice.

## Acceptance

- Main panel shows each agent's tier: master / commit / gate.
- Enter on an agent opens the full-screen detail page (clamped to rows,
  single-line fields); Esc returns to the panel.
- The page shows tool, tier, auto state, invocation, and the
  initial_prompt purpose (or "—").
- Actions (selectable, ⏎ to activate): toggle auto; switch tier
  (commit↔gate) via `set_repo_review`; promote to master via
  `set_repo_master`; remove via `remove_repo_agent` behind the confirm.
- Master's page omits tier-switch / promote / remove; keeps auto.
- DEL on the main panel no longer removes (the action is gone); SPC
  inline auto-toggle still works.
- After any change the panel/page repaint (config.json + agents/ are
  fingerprinted) with no new watch wiring.

## Tests (in-process; no binary spawn)

- Pure routing: Enter on an agent → OpenDetail(idx); Enter on +add →
  picker; the detail page's key fn moves the cursor / activates /
  backs out; master's page exposes a reduced action set.
- Reuse-via-core: activating "promote" calls `set_repo_master` (assert
  `.clank/config.json` master changed + old master demoted);
  "switch tier" calls `set_repo_review` (tier flipped); "remove" calls
  `remove_repo_agent` (gone). Assert via a repo fixture, not a
  reimplemented write.
- Render: tier shown on the panel; detail page clamps to rows and
  one-lines a multiline initial_prompt; selected action is the band.
- Snapshot: `agents` carries master/commit/gate from the two tiers.

## Out of scope

- A real semantic agent-purpose field / "create new agent" (still the
  named future direction).
- Per-entry log selection (separate follow-up plan).
