# tui-agent-auto-toggle
# status --tui: agent roster panel + arm/disarm auto-mode

## Goal

Turn `clank status --tui` from a read-only cockpit into one that can
also **arm/disarm each team agent's auto-mode** in place. Today, to
flip an agent between autonomous (Stop-hook re-arms via `clank wait`)
and parked, you switch into that agent's pane and type `clank auto
on|off`. The TUI already names master and surfaces awaited reviewers;
it should show the whole roster and let you toggle each one's auto
from the keyboard.

## Why this is sound (not a bolt-on)

Auto-mode is **per-agent file state**, not session state:
`crates/cli/src/cli/auto.rs` does `load_agent_config(&repo, &label)` →
set `auto_mode` → `save_agent_config(&repo, &label, …)`. The session
binding (`resolve_identity_from_env`) only chooses *which* label the
CLI writes. So toggling another agent's auto is a plain, label-
addressed config write — a human-run, unbound TUI can do it for any
agent. The stop hook reads the *effective* auto-mode fresh at each
decision (`team::resolve_effective_auto_mode`), so the write is the
whole mechanism; nothing else to signal.

The repaint path is already wired: `status::input_signature`
fingerprints mtimes under `CLANK_WAKE_DIRS`, which **includes
`agents/`**. So both the TUI's own toggle write AND an external
`clank auto` flip bump the fingerprint → `Ev::Refresh` → rebuild →
repaint. No new watch wiring.

No git/gix access — pure `.clank/` config IO — so the
`git_io`/`git_plumbing` boundary is untouched.

## The invariant the UI must not blur

A toggle changes the *armed* state; it takes effect at the target
agent's **next Stop-hook decision**. It does NOT instantly start a
stopped agent, nor kill an in-flight `clank wait`. The panel shows
"auto on/off" = armed/disarmed, and must not read as a live run/stop
button. (Making "off" pre-empt a running wait loop — wait watching its
own config and exiting — is a separate, bigger change and is OUT OF
SCOPE here. If we later want it, it's its own plan.)

If we also show a live-activity column (waiting / working / idle), it
MUST be a visually distinct signal from the armed lamp, so "armed" and
"running right now" are never conflated. Live activity is a stretch;
the minimal version is the armed lamp only.

## Data

Add the roster + each agent's effective auto-mode to `StatusSnapshot`
so it flows through the existing signature-gated rebuild (don't read
configs ad hoc in `render`, or the nothing-changed gate won't cover
them):

- New field e.g. `agents: Vec<AgentRow { label, role, auto_mode }>`,
  built in `StatusSnapshot::build_async` from the resolved `Roster`
  (master + reviewers) + `load_agent_config` +
  `resolve_effective_auto_mode`. Degrade to empty on a teamless repo
  (same as `master: None` does today).
- Not emitted by `to_json` unless trivially free to add — keep the
  `status --json` wire contract unchanged unless a reviewer wants it.

## Refactor: single source for the write

Extract the read-modify-write (preserve `wait_timeout`, only change
`auto_mode`) from `auto.rs::run_on/run_off` into a shared helper, e.g.
`set_auto_mode(repo, label, AutoMode)`. Both `clank auto` and the TUI
toggle call it, so there is one write path.

## Interaction model (the one design fork — recommend, let review refine)

`j`/`k`/`g`/`G`/`SPC`/`b`/arrows already drive **log scroll** today
(`parse_keys`, SPC = PageDown). To honor "select an agent, SPC to
toggle" we need a focus model:

- Recommended: a focus toggle key (`Tab`, or `a` for agents) moves
  focus to the agent panel. While focused: `j`/`k` move a highlighted
  agent cursor, `SPC` toggles the selected agent's auto, `Tab`/`Esc`
  returns focus to the log (restoring `j`/`k`/`SPC` to scroll). Render
  the focused panel with a cursor highlight; render an unfocused hint
  ("Tab: agents").
- Alternative (note, don't default to it): an always-live agent cursor
  with `SPC` meaning toggle whenever an agent is selected — but that
  steals `SPC` from page-down globally. Rejected unless review prefers
  it.

Add a new `Key` variant for the focus toggle and `Ev`/handler arms for
cursor move + toggle; the toggle arm calls `set_auto_mode` then lets
the existing watcher→Refresh path repaint (no manual rebuild).

## Acceptance

- The TUI renders a roster panel: every team agent (master +
  reviewers) with role and an armed/disarmed auto lamp.
- A focus key moves keyboard focus to the panel; `j`/`k` move a
  visible cursor; `SPC` toggles the selected agent's auto and the lamp
  flips on the next frame.
- The flip persists to `agents/<label>/config.json` via the shared
  `set_auto_mode`, preserving `wait_timeout`.
- An external `clank auto on|off` for any agent repaints the panel
  (already covered by the `agents/` fingerprint — assert it).
- Docs/help line in the TUI footer mentions the focus + toggle keys.
- The armed lamp is documented (in code, succinctly) as next-Stop
  semantics, not instant start/stop.

## Tests (in-process cores only — no binary spawn)

- `set_auto_mode` round-trips on/off and preserves `wait_timeout`.
- `build_async` populates `agents` with role + effective auto-mode
  from a fixture roster + per-agent configs (incl. the no-override →
  default case).
- `render`/`render_at` shows the panel, the cursor on the focused row,
  and the correct lamp per agent.
- `parse_keys` recognizes the new focus key; the handler moves the
  cursor within bounds and the toggle arm computes the flipped mode
  for the selected agent.
- `input_signature` changes when an agent's `config.json` mtime moves
  (pins the external-toggle repaint; reuse the existing
  signature-fingerprint test style).

## Out of scope

- Pre-empting a running `clank wait` on disarm (instant stop).
- Waking a stopped agent on arm (instant start).
- Any change to how the stop hook reads auto-mode.
