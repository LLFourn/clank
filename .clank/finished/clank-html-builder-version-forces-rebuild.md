# clank-html-builder-version-forces-rebuild

`clank html` already stamps a builder version into every
index page:

```html
<meta name="clank:builder-version" content="1">
```

`read_prior_head` returns `None` whenever the prior index's
version disagrees with `BUILDER_VERSION`. That correctly
forces (a) a full event re-fold and (b) a full index
re-render.

**The gap**: per-commit pages are still skipped when
`commit/<sha>.html` exists on disk (`writes_needed` filter in
`build_site`). The same applies to per-plan pages, which
also live on disk and were added after the original
versioning landed (`.clank/html/plan/<stem>.html`). After a
`BUILDER_VERSION` bump, the index reflects the new markup
but every old commit/plan page is left untouched — broken
in subtle ways (missing CSS classes, removed JS hooks,
stale layouts, broken relative links).

## Fix

Detect the version-mismatch case explicitly and treat it as
`force_rebuild` for the rest of `build_site`:

1. Refactor `read_prior_head` (or split into two helpers)
   so the caller can distinguish:
   - No prior index → cold build.
   - Prior index with mismatching version → **stale build**.
   - Prior index with matching version → hot incremental.
2. When stale, set the effective `force_rebuild = true` so
   every per-commit AND per-plan page is regenerated.
3. Bump `BUILDER_VERSION` from `"1"` to `"2"` — every
   markup-shape change since v1 landed (sha-copy buttons,
   row split, plan-page links, plan landing pages) means
   v1 outputs do not match v2 output.

The `events.json` cache currently has no version marker.
Good news: it doesn't need one. When `prior_head` returns
`None` (cold or stale), the match in `build_site` already
falls through to the full-fold branch and `write_events_cache`
overwrites the stale JSON. No deletion required.

## When to bump BUILDER_VERSION

Every PR that touches one of:

- `render_row`, `render_umbrella`, `render_timeline`,
  `render_index`, `render_commit_page`, `render_plan_page`.
- The `CSS` or `RELATIVE_TIME_JS` constants.
- `write_doc_open*` / chrome.
- Directory layout under `.clank/html/` (e.g. adding a new
  per-X subdirectory like `plan/`).

Add a short comment above `BUILDER_VERSION` documenting the
contract: "Bump BUILDER_VERSION when the rendered markup,
CSS, JS, or output directory layout changes shape."

## Surfaces touched

- `crates/cli/src/cli/html.rs`:
  - `BUILDER_VERSION` → `"2"` + a comment documenting the
    bump rule.
  - Refactor `read_prior_head` so the caller can tell
    "matching prior" apart from "stale prior" — e.g. return
    a small `enum PriorBuild { Fresh(String), Stale,
    None }` (or two helpers). The `Stale` arm tells
    `build_site` to set `force_rebuild = true` for the rest
    of the build, which already cascades through
    `writes_needed` (per-commit) and `affected` (per-plan).
- No core changes. No new deps.

## Tests

- `html_version_mismatch_rebuilds_every_commit_page` — seed
  a build, then overwrite the
  `<meta name="clank:builder-version" content="2">` tag in
  the index with `"old"`. Touch each existing commit page's
  body (e.g. append a sentinel string) so we can detect
  overwrites. Re-run `clank html`. Assert every commit page
  on disk is rewritten (sentinel string gone).
- `html_version_mismatch_rebuilds_every_plan_page` — same
  shape but assert `.clank/html/plan/<stem>.html` is also
  rewritten when the stamped version doesn't match.
- `html_version_match_preserves_incremental_skip` —
  regression for the existing behavior: a build at the
  current version twice should still skip unchanged
  per-commit pages (mtime unchanged on the older commit's
  page when no slice work landed and it isn't in top-N).

## Out of scope

- Storing the version on each per-commit page in addition
  to the index. The index is the single source of truth for
  the build's identity; commit pages don't need to carry
  it.
- A migration path where v(N-1) pages are kept around for
  comparison. They get overwritten.
- Auto-detecting markup changes at compile time. Bumping
  the version is a manual, intentional act tied to a PR.
