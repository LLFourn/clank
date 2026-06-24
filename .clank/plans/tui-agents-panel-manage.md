# tui-agents-panel-manage
# status --tui: manageable agents panel (focus, play/pause, add/remove)

Builds on the shipped `tui-agent-auto-toggle` panel. Turns the
read+toggle roster into a small management surface: rename it, make the
armed state read as play/pause, make it unmistakable which region has
keyboard control, and let you add a team agent from the global library
or remove one — each mutating action gated by an in-TUI confirm.

## Architecture first: one explicit Mode state machine

The current loop carries `focus: Option<usize>` and resolves each
key's meaning from it. Adding a picker and a confirm dialog on top of
that boolean would scatter "what does this key mean" across the
handler. The backbone of this plan is to replace `focus` with ONE
mode enum that owns key routing — every key is interpreted in exactly
one place per mode:

```
enum Mode {
    LogScroll,                       // default: keys page the log
    AgentPanel { sel: usize },       // cursor in the roster (incl. the +add row)
    AddPicker  { sel: usize },       // choosing a global-library agent to add
    Confirm(Confirm),                // a modal decision is pending
}
struct Confirm { action: PendingAction, default_yes: bool }
enum PendingAction { Add(AgentLabel /* + tool */), Remove(AgentLabel) }
```

`render` takes `&Mode` (not `Option<usize>`); the loop's key handler
`match`es on `&mut mode`. Transitions are the only way state changes:
LogScroll ⇄ AgentPanel (Tab), AgentPanel → AddPicker (Enter on +add) →
Confirm → back to AgentPanel; AgentPanel → Confirm (DEL on a reviewer)
→ back. Esc always backs out one level. This is the correctness
mechanism: a key that isn't bound in the current mode does nothing,
and there is no second place where it could mean something else.

M1 introduces the enum with just `LogScroll`/`AgentPanel` (a
mechanical refactor of today's `focus`); M2 adds the two new variants.

## The committed-config distinction (must be honored, not blurred)

The auto toggle writes `agents/<label>/config.json`, which is
**gitignored** (`.clank/.gitignore` lists `/agents/`) — a personal,
ephemeral, frequent change. Add/remove edits **`.clank/config.json`,
which is TRACKED** — the shared team roster. So an add/remove:
- dirties the working tree with a committed-config change the master
  must eventually commit (exactly what `clank agent add`/`remove`
  already do — the TUI is a front-end to the same cores, not a new
  write path), and
- is a deliberate, rare, team-wide act — NOT the lightweight flip the
  auto lamp is.

That weight is WHY add/remove gets a confirm modal and the auto toggle
does not. The confirm copy names it ("edits the committed team
config"). The panel repaints after the change for free: `config.json`
is already in `CLANK_WAKE_DIRS`, so the snapshot's nothing-changed
gate already covers it.

## Reuse the existing mutation cores (no new write logic)

`crates/cli/src/cli/agent.rs` already has env-free, `pub` cores:
- add by name from the global library:
  `add_repo_roster_agent_by_name(repo, home, &label, role)`
- remove from the repo roster: `remove_repo_agent(repo, &label)`

The TUI calls these — it does NOT reimplement roster editing. The
"available agents" list is the global library minus the current
roster: read `~/.clank` via the existing user-config reader
(`team::read_user_config(home).agents` keys) and subtract
`snapshot.agents`. Each candidate carries its tool (claude/codex) so
the picker shows what you're adding. If the library is empty or every
entry is already on the roster, the picker says so and points at
`clank agent add --global`.

Adds go in as a reviewer (commit tier) — parity with `clank agent
add`, which only ever adds reviewers (master is set via `promote`).
Master is therefore NOT removable from this panel and the +add row
never creates one; DEL on the master row shows a one-line notice
pointing at `clank agent promote`.

## UI design (frontend-design-led)

Goal: in a small monospace pane, make (a) the armed state read at a
glance, (b) the controlling region obvious, (c) the mutating moments
feel deliberate. Use REDUNDANT encoding (shape + colour + position)
rather than any single cue.

### Header rename + section headers

"auto" → "agents". The label outgrows the 5-col gauge gutter, so the
panel graduates to a real section: a one-line header per focusable
region (AGENTS, LOG) that doubles as the focus indicator (below).
This also gives the gutter back its alignment.

### Armed state: play/pause, not a dot

- ON (auto loop running) → ▶ play, green.
- OFF (parked) → ⏸ pause, dim.

Colour stays as a redundant channel (green = playing, dim = paused).

WIDTH TRAP — acceptance-blocking: ▶ (U+25B6) and ⏸ (U+23F8) can
render at different display widths (triangle often 1, pause often 2 /
emoji-presentation), which would jitter the name column in an
alignment-sensitive pane. The implementer MUST verify both glyphs
report the SAME `display_width`; if they don't, pick a consistent pair
(candidates to evaluate: ▶/⏸ with VS15 text-presentation U+FE0E,
►/‖, ▶/❚❚) or pad the narrower into a fixed 2-col mark field (like the
existing verdict MARK_FIELD). A test pins equal width.

### Focus legibility (the core ask)

Three redundant cues mark the region in control:
1. **Left accent rail** — every row of the focused region carries a
   colour `▌` (U+258C) in column 0; the unfocused region carries a
   blank there. A contiguous coloured left edge is the strongest
   "this block is live" signal in a monospace pane.
2. **Header treatment** — the focused region's header is reverse-video
   accent + a filled marker (e.g. `◉ AGENTS`); the unfocused one is
   dim + hollow (`○ agents`). (Reuse/extend the existing
   `Style::Highlight` SGR.)
3. **Footer hint** — names the active region's keys, switching with
   mode: `Tab ⇄ log · ↑↓ move · SPC play/pause · ⏎ add · DEL remove`.

Short panes degrade gracefully: drop the headers first, keep the rail
+ cursor (the rail alone still disambiguates).

### +add button, picker, remove, confirm

ASCII mock (agents panel focused, cursor on codex):

```
○ log ─────────────────────────────────
  ▶ claude     master
▌◉ AGENTS
▌  ▶ claude     master
▌▸ ⏸ codex      reviewer
▌  ▶ ruthless   reviewer
▌  + add agent
  Tab ⇄ log · ↑↓ move · SPC play/pause · ⏎ add · DEL remove
```

- **+ add agent** is the last roster row; it reads as a button (dim;
  accent + underline when the cursor lands on it). ⏎/SPC opens the
  AddPicker.
- **AddPicker** overlays the candidate list (label + tool); ↑↓ + ⏎
  choose, Esc cancels. Choosing → Confirm.
- **Remove**: Backspace/DEL while the cursor is on a reviewer row →
  Confirm. (Master row → inline "use clank agent promote" notice.)
- **Confirm modal** — the high-impact moment: a boxed overlay drawn
  distinctly (box-drawing border + accent title), stating the action
  and that it edits the committed team config, e.g.:

```
┌ confirm ─────────────────────────┐
│ Remove reviewer “codex”?          │
│ Edits the committed team config.  │
│                      [y]es  [N]o  │
└──────────────────────────────────┘
```

  Remove defaults to No (`[y/N]`), add defaults to Yes (`[Y/n]`).
  Keys: y/⏎ confirm, n/Esc/q cancel. On confirm, call the matching
  core; the `config.json` write triggers the watcher → repaint.

## Milestones

- **M1 — focus + glyphs + rename (polish; no new modes).** Mode enum
  with `LogScroll`/`AgentPanel` (refactor today's `focus`); rename to
  AGENTS; play/pause glyphs (width-verified); the focus rail + header +
  footer. Ships the legibility fix fast.
- **M2 — add/remove + confirm (the feature).** `AddPicker` +
  `Confirm` modes; +add row; DEL-to-remove on reviewers; confirm
  modal; wire to the existing cores; candidate list from the global
  library.

## Acceptance

- Header reads "agents"/"AGENTS"; armed state renders as ▶/⏸ with
  green/dim and VERIFIED equal display width.
- The focused region is unmistakable: accent rail on its rows + a
  brightened header + a mode-specific footer; switching with Tab moves
  all three cues.
- +add row opens a picker of global-library agents not already on the
  roster (empty-state message + `--global` hint when none); choosing
  one, after confirm, adds it as a reviewer via the existing core.
- DEL on a reviewer row, after confirm, removes it via the existing
  core; master is never removable here and says so.
- Confirm modal blocks the action until y/Esc, defaults safe for
  remove, and names the committed-config consequence.
- After add/remove the panel repaints (config.json fingerprint) with
  no new watch wiring.

## Tests (in-process; no binary spawn)

- Mode transitions: Tab toggles LogScroll⇄AgentPanel; ⏎ on +add →
  AddPicker; DEL on reviewer → Confirm(Remove); DEL on master → no
  Confirm (notice); Esc backs out each level.
- Render: AGENTS/LOG headers reflect focus (rail present only on the
  focused region; filled vs hollow marker); ▶/⏸ chosen by auto-mode;
  ▶ and ⏸ have equal `display_width`; +add row present; cursor on +add
  styled as active; picker lists library-minus-roster with tools and
  the empty-state message; confirm modal renders the action + the
  committed-config line + the default.
- Candidate computation: global library minus current roster, master
  excluded as a removal target.
- Reuse: the confirm-Yes path calls `add_repo_roster_agent_by_name` /
  `remove_repo_agent` (assert via an in-process repo fixture that the
  roster in `.clank/config.json` changed), NOT a reimplemented write.

## Out of scope

- Promoting/demoting master or changing a reviewer's tier from the TUI
  (stays `clank agent promote` / `set-review`).
- Declaring NEW global-library agents from the TUI (still `clank agent
  add --global`); the picker only surfaces existing ones.
- Auto-committing the config change — the dirtied tree is the master's
  to commit, same as the CLI today.
