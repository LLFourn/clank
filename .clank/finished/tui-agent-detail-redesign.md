# tui-agent-detail-redesign
# Redesign the `status --tui` per-agent detail page

## Problem / core model

The per-agent detail page (`status_tui/render.rs::render_agent_detail`, ~779)
presents a reviewer's **review tier** as a 2-way `commit ↔ gate` toggle
(`detail_row_spans`/`SwitchTier`, render.rs:757; `apply_detail_action`,
mod.rs:145). But the real domain is richer. The roster already models four
review kinds — `ReviewKind::{Commit, Plan, Final, Gate}` (teams_config.rs:291)
— and `set_repo_review` already persists any of them. The 2-way toggle hides
`Plan` and `Final` behind the single word "gate", so two of the four real
states are unreachable from the TUI.

The correct mental model (per lloyd): a reviewer reviews some subset of
**{commit, plan, final}**, shown as checkbox rows:

- `commit` is **exclusive** — a commit-tier reviewer reviews *every* commit,
  which subsumes the gate points; ticking it grays out `plan` + `final`.
- `plan` and `final` are independent checkboxes.
- `plan` + `final` together **is** what the code calls `gate` internally.

So the checkbox states map 1:1 onto the existing `ReviewKind`:
`commit → Commit`, `plan-only → Plan`, `final-only → Final`,
`plan+final → Gate`. This is presentation catching up to a model the data
layer already has — not new domain.

The page also has four smaller defects, all in the same detail render.

## Scope

### a. Review tier as checkbox rows (the main change)
Replace the `commit/gate` toggle with three checkbox rows `commit` / `plan` /
`final`:
- `commit` exclusive: when ticked, `plan`/`final` render grayed/disabled.
- `plan`/`final` independent; both ticked == `Gate`.
- Toggle in place with Space / ← / → on the focused row (input.rs
  `agent_detail_nav`), mapping the resulting checkbox set → `ReviewKind` and
  persisting via `set_repo_review`.
- Master agents have no review tier — hide the rows (or render "master, n/a").

### b. Remove the `purpose` row
`agent.description` / `initial_prompt` is unset for every real agent and there
is no auto-fill, so it renders a permanent "—". Drop the `purpose` info row
(render.rs:796–810). Leave the field on the config type; just stop surfacing
it here. If `AgentAutoRow.description` becomes unused, drop it too.

### c. Copy-pastable invocation
The invocation is `one_line(...)`-truncated with `…` (render.rs:803), so a long
launch command can't be copied. Render it in full on its own `wrap()`-ped
line(s) (text.rs:79) under an `invocation:` label so the whole command is
visible and selectable — no ellipsis on the command itself.

### d. Tier change must not exit the page
`apply_detail_action`'s `SwitchTier` returns `Mode::AgentPanel { sel: idx }`
(mod.rs:150), closing the detail page on every change. Keep
`Mode::AgentDetail { idx, sel }` after a tier edit so the user can tick
several boxes and watch the result without being kicked out.

### e. Show session binding
Each agent's session lives at `.clank/agents/<label>/config.json` as
`AgentConfig.session: Option<Session>` (core/agent_config.rs — id/tool/
updated_at) but is never shown. Load it into `AgentAutoRow` and render a
`session:` row:
- bound → show the session id (dim).
- unbound (`None`) → render it as a **problem**: red + a `✗` glyph, because an
  unbound agent can't receive work.

## Files
- `crates/cli/src/cli/status_tui/render.rs` — `render_agent_detail`,
  `detail_row_spans`, `toggle_segment`, `tier_label`.
- `crates/cli/src/cli/status_tui/input.rs` — `DetailAction`, `detail_actions`,
  `agent_detail_nav`.
- `crates/cli/src/cli/status_tui/mod.rs` — `apply_detail_action`.
- `crates/cli/src/cli/status.rs` — `AgentAutoRow` (+session, drop description
  surfacing), `agent_invocation`.
- `crates/cli/src/cli/status_tui/text.rs` — `wrap`/`one_line` already exist.

## Acceptance
- Detail page shows `commit`/`plan`/`final` checkbox rows; `commit` exclusive;
  toggling maps to the right `ReviewKind` and persists.
- Toggling a tier box does NOT leave the detail page.
- No `purpose` row.
- Invocation shown in full (wrapped), copy-pastable, no `…` on the command.
- `session:` row shows the bound id, or red `✗ unbound` when not bound.
- Master agents show no tier checkboxes.
- Tests (in-process cores, no binary spawn): checkbox-combo → `ReviewKind`
  mapping; render assertions for the new rows.

## Open questions
- OQ1: empty-tier state — unticking the last remaining box should be a no-op
  (must review at least one thing) vs. snapping back to `commit`. Lean: no-op.
- OQ2: confirm `wait.rs` gating treats `Plan` and `Final` distinctly (the data
  model has them; verify the gate logic honors each, so exposing them is real).
- OQ3: invocation as a wrapped full-width block is simplest and copy-pastable;
  reject OSC-8 (it's a shell command, not a URL).

## Implementation notes (as built)

- OQ1 resolved as leaned: unticking the last remaining coverage is a NO-OP
  (`tier_after_toggle` returns None) — a reviewer must review something.
- OQ2 verified before implementation: `compute_gate` takes distinct
  plan-tier/final-tier reviewer lists (wait.rs), so the checkboxes expose
  real gating behavior.
- Grayed plan/final rows (while commit is ticked) are dim but STILL
  toggleable — ticking one LEAVES commit mode (otherwise commit would be a
  trap state). Checkbox glyphs are ASCII `[x]`/`[ ]` (guaranteed monospace).
- The pure state machine lives in input.rs (`tier_boxes`,
  `tier_after_toggle`, `role_review_kind`, `review_kind_role`) with a full
  12-transition matrix test.
- Stay-on-page (defect d): the snapshot row's role is updated in place and
  the mode stays `AgentDetail`; the existing label-based `relocate_detail`
  covers concurrent external refreshes.
- Session row (defect e): `AgentAutoRow.session` loaded from the same
  `AgentConfig` read the row builder already does; unbound renders red
  `✗ unbound — run `clank as <label>` in its session`.
- `AgentAutoRow.description` dropped entirely (purpose row removed); the
  picker's `AvailableAgent.description` is untouched.
