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

1. Restructure `parse_diff` so that, for each file section, the path comes from the `--- ` / `+++ ` lines rather than the `diff --git` line.
   - Treat `diff --git ...` purely as a "start of next file" boundary.
   - When `--- a/<path>` appears, take everything after `a/` to end-of-line as the old path; same for `+++ b/<path>`.
   - Handle `/dev/null` on either side (additions/deletions) as today.
2. Keep `parse_diff_git` only if it's still useful as a boundary detector; otherwise inline the prefix check.
3. Add unit tests in `diff_parser.rs` covering:
   - Path with a single space (`foo bar.txt`).
   - Path with multiple spaces and a leading space.
   - Rename header (`diff --git a/old name.txt b/new name.txt` followed by `rename from` / `rename to`) — confirm both old and new are captured correctly from the `---`/`+++` lines.
   - Addition (`/dev/null` → `b/foo bar.txt`) and deletion (mirror).
   - Existing no-space cases still pass (regression guard).

## Out of scope

- `core.quotePath` C-escape decoding (note as a known limitation if not addressed).
- Any changes to `FileDiff` shape or downstream consumers.

## Acceptance

- New unit tests pass.
- Existing tests in `crates/cli` still pass (`cargo test -p clank-cli`).
- A `git diff` over a file with a space in its path produces a `FileDiff` whose `path` equals the real path on both old and new sides.
