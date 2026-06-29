# adhoc-commit-marker
# Mark ad-hoc commits with an inline icon instead of a segregated bucket

## Problem / current display

An ad-hoc commit (`LogEvent::AdHoc` — a commit with no `[plan]` tag) is
currently grouped under its OWN umbrella section: `umbrella_sections`
maps it to `UmbrellaKey::AdHoc`, which `oneline_rows`
(`crates/cli/src/cli/log.rs`) renders as `OnelineRow::Header { plan:
None }` displayed as the literal "adhoc" header (log.rs ~381/406). So
ad-hoc commits are visually walled off into a separate "adhoc" block,
both in `clank log --oneline` and in `clank status --tui` (which renders
the same `OnelineRow`s via `build_scroll` / the log-row spans).

We'd rather ad-hoc commits appear in the NORMAL commit flow — in their
natural chronological place under the surrounding plan's umbrella —
each tagged with a small, distinct icon so you can still tell at a
glance that the commit is ad-hoc / miscellaneous (not part of a tagged
plan).

## Goal

In BOTH `clank status --tui` and `clank log --oneline`, an ad-hoc commit
renders as an ordinary commit row PLUS a per-row marker icon (a single
UTF-8 glyph in a distinct color — yellow was the user's first instinct)
indicating "miscellaneous / ad-hoc". Keep the two surfaces consistent.

The user's framing: "appear normally under the umbrella of the plan, but
with a [yellow] icon next to them to indicate they are ad-hoc. Maybe not
a warning — some icon to indicate they are miscellaneous."

## OPEN DECISION — glyph + color (reviewers/user pick at intro)

Concrete candidates proposed for the intro review. All are **width-1**
and widely rendered (the width trap below is real); color is yellow
(`33`) per the user's instinct, dim as a fallback. Sample row:
`a1b2c3d <glyph> tweak error message wording`

| glyph | reads as | notes |
|------|----------|-------|
| `~`  | "misc / loose / approx" | ASCII, GUARANTEED width-1, never an "error" — **my recommendation** |
| `*`  | "note / aside" | ASCII width-1; slight "footnote" feel |
| `◇` (U+25C7) | "uncategorized marker" | prettiest, but East-Asian-**ambiguous width** in some terminals — must verify the gutter holds |
| `·` (U+00B7) | "minor / misc" | very subtle; may read as filler |

Recommendation: **`~` in yellow** — ASCII-safe (no width risk), clearly
not an error, reads as "miscellaneous/loose". `◇` only if reviewers
accept the ambiguous-width verification cost. The reviewers (and the
user) pick; record the decision in the implementation commit.

Avoid a literal warning/error glyph if it reads as "something is wrong"
— ad-hoc is not an error, just uncategorized. Pick something that reads
as "miscellaneous", is widely-rendered (no exotic emoji that terminals
render at inconsistent width), and looks right in both the TUI's
monochrome-ish panel and plain `--oneline` output.

## Implementation notes / design points

- **The row must carry ad-hoc-ness per commit.** `OnelineRow::Commit {
  sha, subject }` has no ad-hoc flag today — ad-hoc-ness is conveyed
  only by the `Header { None }` bucket. To mark individual rows, either
  add an `ad_hoc: bool` to `OnelineRow::Commit` (derived from
  `LogEvent::AdHoc` in `oneline_rows`) or thread the umbrella context
  through. The pure `oneline_rows` producer is the single source both
  surfaces consume, so do it there once.
- **Where ad-hoc commits sit.** Decide whether to drop the separate
  "adhoc" `Header` entirely (folding ad-hoc commits into the
  surrounding plan umbrella / the normal flow) or keep some grouping —
  confirm against the `umbrella_sections` data model
  (`crates/core/src/repo_state.rs`; ad-hoc commits live in `ad_hoc`).
  The goal is that they read as normal rows with a marker, not a walled
  block.
- **WIDTH TRAP.** The marker is a leading/trailing glyph in a row that
  is width-budgeted. Use the existing `display_width` / `char_width`
  primitives and a FIXED-width gutter so a marked row and an unmarked
  row stay column-aligned (this codebase has been bitten by ambiguous-
  width glyphs before — pick a width-1 glyph and verify alignment).
- **Color.** Reuse the existing span/color helpers (e.g. `colored("33",
  ..)` for yellow in `status_tui/text.rs`); for `--oneline`, match the
  existing ANSI-color convention used by the other log rows.
- **`--json`.** `LogJsonRow::AdHoc` already distinguishes ad-hoc in the
  `--json` output (kind: "ad-hoc"); keep that untouched (the icon is a
  human-display concern only).

## Acceptance criteria

- An ad-hoc commit renders inline as an ordinary commit row with the
  chosen marker icon (+ color) in BOTH `clank log --oneline` and `clank
  status --tui`, consistently.
- A non-ad-hoc (plan-tagged) commit renders WITHOUT the marker —
  unchanged.
- The marker is produced once in the pure `oneline_rows` layer (single
  source of truth for both surfaces); a unit test pins that an
  `OnelineRow` for an ad-hoc commit is marked and a plan commit is not.
- Column alignment holds (marked vs unmarked rows; via `display_width`).
- `--json` output is unchanged.
- The icon glyph + color were proposed by the implementer and chosen by
  the reviewers (record the decision in the implementation commit).

## Out of scope

- Changing what COUNTS as an ad-hoc commit (the `LogEvent::AdHoc`
  classification / the `ad_hoc` model) — display only.
- The `--json` schema (unchanged).
- Any reordering of the timeline beyond folding ad-hoc rows into the
  normal flow.
