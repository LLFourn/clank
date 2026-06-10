# log-plan-umbrellas — group log output under plan umbrellas + color the TUI log

Both `clank log --oneline` and the status TUI's live-log pane
currently print a flat commit list with raw `[plan]`-prefixed
subjects. Arrange them like the HTML timeline instead (lloyd
2026-06-10): the PLAN is an umbrella header containing its
commits, and inside the umbrella each commit's `[plan]` prefix is
STRIPPED (it's redundant under the header). Also: the TUI log
should look NICE — use color, e.g. the utf8 verdict ticks
(✓ green, ✓✓ cyan, ✗ red) and dim shas, instead of all-dim lines.

## Donors (verified)

- `html.rs:render_timeline`/`render_umbrella`/`umbrella_key`
  (~:620-680): the umbrella algorithm already exists — walk
  newest-first, group CONTIGUOUS same-plan events into one
  section, ad-hoc commits get their own kind. Reuse the
  grouping logic (factor it if practical; it's small enough to
  mirror if the html shapes don't lift cleanly).
- `repo_state::parse_subject` (core :337): pure `[prefix] body`
  splitter — strip the prefix inside umbrellas with this, never
  string-munging.
- `log.rs:oneline_rows` (status-tui-live-log): the structured-rows
  layer is the right place to introduce an umbrella row kind —
  e.g. `OnelineRow::PlanHeader { plan }` + commits carrying
  `subject_stripped`. Both renderers (CLI print + snapshot plain
  lines) and the TUI styling consume the same rows.

## Sketch

```
teams-based-agent-registration
  f6feba2 intro
    ✓ codex: well scoped
  83b7c2d implement registration set
    ✓✓ codex: ship it
adhoc
  ffb94b9 remove all binary-spawning tests
```

- Umbrella header = plan stem (or `adhoc`); contiguous grouping
  per the html rule (a plan interrupted by another plan's commit
  opens a new umbrella — chronology is never reordered).
- Inside: `[plan]` prefix stripped via parse_subject; commits
  indented one level, reviews two.

## TUI color

The TUI log tier currently renders every line dim. With
structured rows reaching the snapshot (not pre-flattened
strings), the TUI can style per row kind via its existing span
model: plan headers plain/bold-ish, shas dim, subjects plain,
verdict marks in their CLI colors (green ✓ / cyan ✓✓ / red ✗),
authors dim. NOTE: this changes the snapshot field from
`log_lines: Vec<String>` to structured rows (e.g.
`Vec<OnelineRow>`) with the plain-text flattening moving to the
consumers — the CLI keeps `oneline_plain_lines`, the TUI maps
rows -> spans. The pure-formatter discipline holds: rows carry
data, renderers own presentation (incl. ANSI).

`clank log --oneline` keeps its existing color scheme, now with
umbrella headers + stripped subjects.

## Tests

In-process, per the standing rule: rows-level unit tests
(umbrella grouping incl. the contiguity rule + interleave split,
prefix stripping, adhoc umbrella), TUI pure-render tests for the
styled log tier (existing strip-ANSI helpers), and the
status_log_integration assertions updated to the umbrella shape.

## Out of scope

- Changing `clank log` (human/full) or `--json` shapes beyond the
  oneline path.
- The html timeline itself (already umbrella'd).

## Status

Stub — queued lloyd 2026-06-10. Builds directly on
status-tui-live-log (the rows layer it introduced); land after
that plan finalizes.
