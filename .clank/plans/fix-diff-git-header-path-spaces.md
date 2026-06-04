# fix-diff-git-header-path-spaces
# Fix `diff --git` header parser for paths containing spaces

## Problem

`crates/cli/src/diff_parser.rs:122-128` (`parse_diff_git`) extracts the old/new path from the `diff --git a/<path> b/<path>` header using `split_whitespace()`. Git emits this header unquoted for ordinary paths containing spaces, so for `diff --git a/foo bar.txt b/foo bar.txt` the parser produces `("foo", "bar.txt")` instead of `("foo bar.txt", "foo bar.txt")`.

Downstream consequences:
- `FileDiff.path` is wrong, so `finalize()` / `is_always_folded()` see the wrong path.
- Any review-UI / feedback-metadata code keyed off `FileDiff.path` mis-attributes hunks.

## Root cause / architecture

The `diff --git` line is genuinely ambiguous when paths contain whitespace — there is no way to recover the split without knowing the path length. The cleaner model is to treat `diff --git` as a section marker only and source the canonical paths from the `--- a/<path>` and `+++ b/<path>` lines that follow each header. Those lines have a single prefix delimiter and the path runs to end-of-line, so they parse unambiguously.

(Secondary concern: git's `core.quotePath` C-escapes some paths. Not in scope for this fix unless trivial to address; if not, leave a comment noting the limitation.)

## Approach

The path for each file section is gathered from up to four possible sources, in this priority order, so each git-diff shape produces a correct path:

| Source line                                | When it fires                  | Reliable for spaces?         |
|--------------------------------------------|--------------------------------|------------------------------|
| `+++ b/<path>` (path runs to EOL)          | normal, addition               | yes                          |
| `--- a/<path>` (path runs to EOL)          | normal, deletion               | yes                          |
| `rename to <path>` / `rename from <path>`  | pure renames (no ---/+++)      | yes                          |
| `Binary files a/<old> and b/<new> differ`  | binary files (no ---/+++)      | only if path lacks " and "   |
| `diff --git a/<old> b/<new>` (last resort) | nothing else fired             | no — same bug as today       |

1. Restructure `parse_diff` so the canonical path for a section is set when we see `+++ b/<path>` (preferred) or `--- a/<path>` (for deletions or as fallback). Take everything after the `a/` / `b/` prefix to end-of-line.
2. `diff --git` becomes a boundary detector only — it starts a new `FileDiff` but does NOT set the path. The `parse_diff_git` function (or the inlined prefix check) returns Option<()> rather than Option<(String, String)>.
3. Renames are unchanged from today: the existing `rename from` / `rename to` handlers (lines 50-58) already source paths from those lines unambiguously. Pure renames (similarity 100%, no `---`/`+++`) keep working via that path; renames with content changes will get the path from `+++`/`---` as for any modified file.
4. Binary files: parse the `Binary files a/<old> and b/<new> differ` line for the path. The ` and ` separator is the only reliable split, with the known limitation that a path containing the literal substring ` and ` will mis-parse — flag this in a code comment and a TODO.
5. Fall-through: if a file section has none of the above (shouldn't happen for valid `git diff` output, but be defensive), keep a best-effort `parse_diff_git` parse so we don't lose the file entirely.

## Tests

In `diff_parser.rs`:

- Path with a single space (`foo bar.txt`) — modified file.
- Path with multiple spaces and a leading space (`  foo  bar.txt`) — modified file.
- Pure rename with spaces: `diff --git a/old name.txt b/new name.txt` + `similarity index 100%` + `rename from old name.txt` + `rename to new name.txt`. Verify old/new path captured from `rename from`/`rename to`. NO `---`/`+++` lines present.
- Rename WITH content changes and spaces: `rename from` + `rename to` + `--- a/old name.txt` + `+++ b/new name.txt`. Verify old/new captured.
- Addition (`/dev/null` → `b/foo bar.txt`).
- Deletion (`--- a/foo bar.txt` → `/dev/null`).
- Binary file with a space (`Binary files a/foo bar.bin and b/foo bar.bin differ`). Verify path.
- Regression: existing no-space cases (already in the test module) still pass.

## Out of scope

- `core.quotePath` C-escape decoding for paths with control chars or non-ASCII when `core.quotePath=true` (the default). Note as a code comment + TODO.
- Binary-file paths containing the substring ` and ` (e.g. `safe and sound.bin`). Note as a code comment + TODO.
- Any changes to `FileDiff` shape or downstream consumers.

## Acceptance

- New unit tests pass.
- Existing tests in `crates/cli` still pass (`cargo test -p clank-cli`).
- A `git diff` over a file with a space in its path produces a `FileDiff` whose `path` equals the real path on both old and new sides.
