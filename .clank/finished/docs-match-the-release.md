# docs-match-the-release
# Docs match the release

## Problem

Lloyd is about to cut a release and wants every doc up to date. The book's own
note said the README still claimed "no daemon" long after the per-repo monitor
existed. That was fixed by hand on 2026-10-29, but nothing caught it: the gates
(`it/book_is_true.rs`, the README reference test in `cli/command.rs`) check
specific claims, not whether the prose still describes clank as it is. This is
the `docs-match-the-release` blocker in RELEASE-CHECKLIST.md §1.

## Change

1. **Audit every user-facing doc against the code as it is now**: README.md, every
   `book/*.html` chapter, `GROK-INTEGRATION.md`, `docs/cli-audit.md`, the skills
   that `clank setup` installs, `--help` text, and setup/doctor summaries.
   - For each claim about behaviour (commands, flags, defaults, config keys,
     hooks, the monitor, recovery and nudges, roles, finish/autosquash, remote),
     check it against the source, and fix or delete it.
   - Delete notes in the book that point at stale text elsewhere ("X is out of
     date"). Fix the stale text instead.
   - Cover what landed recently and may be undocumented: the version in the TUI
     pane name; the `unwatched` status and the stall watchdog
     (`recovery.stall_warn`, `recovery.stall_nudge`); the PostCompact hook and
     re-running `clank setup`; the `⁂` bulk-commit log row; Linux CI support.
2. **Fix the checklist's known items** under `docs-match-the-release`:
   - setup's summary omits OpenCode
   - `open-worktree.sh` and the hidden `open dry`: delete them if nothing else
     uses them
   - `GROK-INTEGRATION.md`: fold it into the README or book, or move it under
     `docs/`
   - plus the rest of that checklist entry
3. **Make drift harder where it's cheap.** Where a claim names a command, flag,
   config key or file path, prefer a gate that already exists (extend
   `book_is_true` or the README reference test) over new prose. Don't build a
   new doc-testing framework.

## Out of scope

- Rewriting the book's structure or design.
- Release packaging itself (the other §1 blockers).

## Acceptance

- Each doc listed above has been read in full against the code. The commit body
  lists, per doc, what was wrong and what changed.
- The checklist's `docs-match-the-release` items are all resolved, and the
  checklist is ticked.
- `book_is_true`, the README reference test and `cli::command` pass. Any gate
  extended in (3) fails when the claim it checks is reverted (mutation).
- No "this is out of date" notes remain anywhere in the docs.
