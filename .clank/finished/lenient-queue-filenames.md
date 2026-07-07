# lenient-queue-filenames
# Lenient queue filenames: missing priority prefix defaults to 999

## Why (lloyd)

`.clank/queue/` files must be named exactly `NNN-name.md` (three-digit
prefix, dash at byte 3 — scan_queue, queue.rs:188). Anything else —
`foo.md` dropped in by hand, `12-foo.md` — is SILENTLY invisible: not
listed, not promoted, no error. A hand-authored queue file should just
work; the strict shape is a writer convention, not a reader contract.

## Parse rule (the whole change)

For a `*.md` file in the queue dir, split the stem at the FIRST `-`:

- left side is 1–3 ASCII digits → that's the priority (so `12-foo.md`
  = priority 12, name `foo` — today invisible), zero-padding optional;
- otherwise the ENTIRE stem is the name with default priority **999**
  (end of the queue: explicit priorities always sort sooner). So
  `foo.md` = `foo` @ 999, `2fa-support.md` = `2fa-support` @ 999 (the
  left side `2fa` isn't all digits), `1234-foo.md` = `1234-foo` @ 999
  (four digits exceed the 0–999 range, so it's a name, not a bad
  priority).

Deterministic and total: every `.md` in the dir is now a queue entry.
Sorting stays (priority, name).

## What stays canonical

- WRITERS are unchanged: `queue add` still writes `{:03}-name.md`;
  `stash push --to-queue` likewise. Lenience is read-side only.
- Reprioritise (`clank queue reprioritise`, TUI +/-) renames to the
  canonical `NNN-name.md` — nudging a bare `foo.md` canonicalizes it
  as a side effect (verify the rename path handles a source file with
  no prefix; it operates on the scanned entry's own path).
- `queue list` prints the canonical `{:03}-name` rendering — a bare
  file lists as `999-foo`, which also teaches the convention.
- Duplicate-stem detection (scan_queue_no_dups): `foo.md` +
  `500-foo.md` now BOTH scan to name `foo` and must trip the existing
  duplicate error rather than racing (pin with a test).
- Update the stale comment on stash push's --priority validation
  ("scan_queue only recognizes three-digit priorities").

## Tests (pure over a tempdir queue)

- `foo.md` → (`foo`, 999); `12-foo.md` → (`foo`, 12); `012-foo.md` →
  (`foo`, 12); `1234-foo.md` → (`1234-foo`, 999); `2fa-support.md` →
  (`2fa-support`, 999).
- Bare files sort after explicit priorities; ties by name.
- Bare + prefixed same stem → duplicate error from scan_queue_no_dups.
- Reprioritise on a bare file renames it to the canonical prefix.
- Promote of a bare-named entry works end-to-end (wait's
  PromoteFromQueue path reads the same scan).

## Non-goals

Changing the default add priority (500), the 0–999 range, or writer
formats; recursive queue dirs; non-`.md` files (still ignored).

## Acceptance

Dropping `foo.md` into `.clank/queue/` makes it visible to `clank
queue`, the TUI QUEUE section, html, and promotion at priority 999;
clippy/fmt/suites green.
