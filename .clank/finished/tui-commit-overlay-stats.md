# tui-commit-overlay-stats
# TUI commit overlay: diff stats before the message, hash above the title

## Why (lloyd)

Two requests for the commit overlay in `clank status --tui`:

1. Show the files changed with lines added/removed BEFORE the commit
   message content — the shape of a change first, prose second.
2. The commit title must not be indented by the short hash. Today the
   subject wraps under a sha-width gutter (`build_commit_lines`
   render.rs:995 builds `<short_sha>  <subject…>` with continuations
   aligned to the gutter). Put the hash on its own line ABOVE the
   title; the title wraps flush-left at full width.

## Layout (top to bottom)

```
 COMMIT ──────────────────── ↑↓ scroll · o browser · Esc back ──

 5e2c4fa1bbbe
 warn the master at HEAD when a second plan opens before the
 first is finished

 crates/cli/src/preview.rs                        +18  -1
 crates/cli/tests/finish_integration.rs           +96  -0
 2 files · +114 -1

 <commit message body…>

 <reviewer feedback blocks, as today>
```

- Hash line dim (it's identity, not content); title plain/bold,
  full-width wrap.
- Stat block: one row per file, counts right-aligned; `+n` accent,
  `-n` red; long paths middle-truncated KEEPING the filename; summary
  line dim. The overlay already scrolls, so large commits just scroll.
- `commit_review_offset` stays derived from the same
  `build_commit_lines` layout (its contract), so review-jump offsets
  shift correctly with the new rows — no separate computation.

## New read: per-commit numstat

`git_io` needs `commit_numstat(sha) -> Vec<{path, added, removed}>`
(+ binary-file handling: git shows `-` — render as `bin`). Prefer gix
(tree diff with blob line counts); if gix can't produce it faithfully
yet, a subprocess read INSIDE git_io with the standard one-line
"why not gix" note. Fetched in `fetch_commit_detail` alongside
subject/body so a background Refresh keeps it live; carried on
`CommitDetail`.

## Tests (pure)

- `build_commit_lines` ordering pinned: rule → hash line → title →
  stats → body → reviews (find-index assertions like the stash-order
  tests).
- Title wraps flush-left (no gutter); hash line contains only the sha.
- Stat rows render counts and the summary line; binary file renders
  `bin`; long path keeps its filename.
- Existing subject-wrap/gutter tests updated to the new shape.
- `commit_review_offset` still lands on a reviewer block with stats
  present.

## Non-goals

No per-file drill-in/diff view; no change to `o` (browser) or review
blocks; no stats on other overlays (plan/queue/stash docs have no
diff).

## Acceptance

Opening any commit in the TUI shows hash-above-title flush-left and
the stat block before the body; reviewer jump still lands right;
clippy/fmt/suites green.
