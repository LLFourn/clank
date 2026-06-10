# zellij-layout-config-around-agent-panes

Let the user configure the zellij layout *chrome* that wraps
clank's agent panes, in `~/.clank/config.json`. clank owns only
the agent pane group (master + reviewers); everything around it
(tab bars, status bars, theme, extra panes, tab template) comes
from the user's config.

Replaces the abandoned `open-zellij-inherits-default-layout`
plan (lloyd 2026-06-09). That plan tried to INHERIT chrome by
shelling out to `zellij setup --dump-layout default` and
splicing clank's tab into it — which dragged in a thorny
"preserve vs always-strip the user's existing tabs" design
question that blocked it. This approach sidesteps that entirely:
the user explicitly authors the wrapper in clank's own config,
so there's no dump-layout parsing and no ambiguity about what to
keep or strip.

## Problem (carried over from the old plan)

`clank open zellij` emits a fully self-contained layout with the
`zellij:tab-bar` / `zellij:status-bar` plugin panes nailed
inline inside the clank tab. Consequences:

1. Tab chrome is hard-coded in clank — the user's theme, plugin
   aliases, custom bars (e.g. `compact-bar`) are ignored.
2. New tabs spawned at runtime drop the bars entirely (the bars
   are panes of clank's tab, not a `default_tab_template`).

## Approach

Add a user-scope zellij layout config to `~/.clank/config.json`
(it's a personal UI preference, same across repos). The user
provides a KDL layout TEMPLATE with a marker showing where the
agent panes go; clank substitutes the composed agent pane group
into the marker and writes the result to
`<repo>/.clank/zellij/layout.kdl` as today.

Sketch (exact shape to pin at sizing):

```json
{
  "zellij": {
    "layout": "layout {\n    pane size=1 borderless=true { plugin location=\"tab-bar\" }\n    __CLANK_AGENTS__\n    pane size=1 borderless=true { plugin location=\"status-bar\" }\n}"
  }
}
```

clank replaces the `__CLANK_AGENTS__` marker with the
`pane split_direction="horizontal" { <master> <reviewers...> }`
group it already composes. The user controls bars, splits, tab
template, theme — all of it.

## Open questions to pin when sized

- **Marker mechanism**: a sentinel node like
  `pane name="__clank_agents__"` that clank replaces, vs a
  string token, vs a dedicated KDL node clank looks for. A real
  KDL parse (the `kdl` crate, as the old plan settled on) is the
  robust choice over string munging — the template is user
  input and KDL strings can contain `{`/`}`. Decide: parse +
  tree-substitute, or documented string-token replace.
- **No marker present**: error ("your zellij.layout must contain
  the agent marker") vs append the agent group as a sibling.
  Lean: error — silent placement guesses are surprising.
- **No `zellij` config at all**: fall back to today's built-in
  self-contained layout (bars inline). Keep current behavior as
  the zero-config default.
- **Repo-scope override?** Probably user-scope only to start
  (matches `~/.clank/config.json` as the user asked). A
  repo-scope override is a later add if needed.
- **Validation**: validate the template parses as KDL at config
  load / `clank doctor` time so a broken template fails early,
  not at `open zellij` time.

## Surfaces

- `crates/cli/src/cli/open_zellij.rs` — `compose_kdl` / the KDL
  composition: read the user's `zellij.layout`, substitute the
  agent group, fall back to the built-in when unset.
- User config schema (`teams_config::UserConfigFile` or a
  sibling section) — add the `zellij` section.
- `clank doctor` — optionally validate the template.

## Promote-time notes (verified against the code)

- Surfaces check out: `compose_kdl` (open_zellij.rs:111) builds
  the layout with `zellij:tab-bar`/`zellij:status-bar` inlined at
  :117/:126 — exactly the hard-coding this plan removes. It
  already has pure in-process unit tests (:206+); the template
  substitution gets the same treatment (NO binary-spawning tests
  — test the composition subroutine, per the standing rule).
- No `kdl` crate dep exists today; adding it is a real decision
  for the marker-mechanism question, not already-paid cost.
- `UserConfigFile` (teams_config.rs:28) has typed sections +
  `extra` passthrough — a typed `zellij` section slots in
  alongside `review`/`hooks`/`diff`.

### Dissolves the TUI-status-pane follow-on

`clank-status-tui` (finished) deferred "add a --tui status pane
to the generated layout" as a separate plan. With THIS plan the
user simply authors it in their template chrome:

```kdl
pane size=8 { command "clank"; args "status" "--tui" }
```

…next to the `__CLANK_AGENTS__` marker. Ship that as the
documented example template — it showcases the feature AND
delivers the status pane without clank hard-coding it. No
separate plan needed.

## Out of scope

- Inheriting from `zellij setup --dump-layout` (the abandoned
  approach).
- Per-pane command customization beyond the existing
  `clank agent start <label>` invocation.

## Decisions (sized, ruthless fb3f85f concerns folded)

1. **Marker = sentinel NODE + kdl-parse tree-substitution**
   (concern 1, load-bearing). A `clank_agents` node; clank parses
   the template with the `kdl` crate, depth-first replaces every
   marker node with the composed agent pane group, re-serializes.
   No string-replace on user-authored KDL: a node can't
   false-match comments or string literals (pinned by test:
   `marker_inside_comment_or_string_is_not_substituted`), and the
   substituted result is valid KDL by construction. Dep pinned to
   `kdl = "4"` — KDL v1, the SAME dialect zellij itself parses
   (kdl 6.x defaults to KDL 2.0, which zellij does not read).
2. **Problem #2 fixed for zero-config users too** (concern 2,
   option b): the BUILT-IN layout became a template using
   `default_tab_template`, so bars apply to runtime-spawned tabs
   out of the box — and the built-in flows through the SAME
   substitution path as user templates (one code path; the
   built-in IS a template with the marker).
3. **Validation early** (concern 3): `validate_template` = parse
   + marker-present; result-validity is by construction (tree
   substitution), so those two checks are the whole contract.
   Wired into `clank doctor` (runs BEFORE the team-resolution
   early-returns, so a broken template surfaces even on a
   teamless repo), failing with the kdl parse error or a message
   naming `clank_agents`.

AS SHIPPED:
- `teams_config::ZellijSection { layout }` on `UserConfigFile`
  (user-scope only), rustdoc carries the documented example —
  compact-bar chrome + a `clank status --tui` pane next to the
  marker (the TUI-status-pane dissolve).
- `open_zellij`: `BUILT_IN_TEMPLATE` (default_tab_template +
  marker), `agent_group_kdl`, `substitute_marker`/
  `substitute_in_doc` (depth-first, splices at marker position,
  counts replacements, errors at 0 naming the marker),
  `validate_template`, `compose_kdl(.., user_template)`.
  `run()` reads `~/.clank/config.json#/zellij/layout`.
- Tests (all in-process, no binary spawning): 22 in
  open_zellij (15 pre-existing pass unchanged through the new
  pipeline — built-in output equivalence; 7 new: built-in
  default_tab_template + parses, chrome preserved + marker
  substituted + user owns bars, nested marker, no-marker error,
  invalid-KDL error, comment/string no-false-match, validate
  matrix) + 4 doctor tests (broken/marker-less/valid/absent).

## Status

Sized + implemented 2026-06-10 (replaces
open-zellij-inherits-default-layout, queued 2026-06-09).
