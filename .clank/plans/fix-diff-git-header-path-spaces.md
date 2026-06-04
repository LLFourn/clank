# fix-diff-git-header-path-spaces
# Fix `diff --git` header parser for paths containing spaces

## Problem

`crates/cli/src/diff_parser.rs:122-128` (`parse_diff_git`) extracts the old/new path from the `diff --git a/<path> b/<path>` header using `split_whitespace()`. Git emits this header unquoted for ordinary paths containing spaces, so for `diff --git a/foo bar.txt b/foo bar.txt` the parser produces `("foo", "bar.txt")` instead of `("foo bar.txt", "foo bar.txt")`.

Downstream consequences:
- `FileDiff.path` is wrong, so `finalize()` / `is_always_folded()` see the wrong path.
- Any review-UI / feedback-metadata code keyed off `FileDiff.path` mis-attributes hunks.

## Root cause / architecture

The `diff --git` line is genuinely ambiguous when paths contain whitespace — there is no way to recover the split without knowing the path length. The cleaner model is to treat `diff --git` as a section marker only and source the canonical paths from the `--- a/<path>` and `+++ b/<path>` lines that follow each header.

Important: these lines are NOT "path runs to end-of-line". When the path contains whitespace (or in certain other cases), git delimits the path from optional trailing metadata (timestamps, etc.) with a literal TAB character. The canonical format is:

```
--- a/<path>\t[optional metadata]\n
+++ b/<path>\t[optional metadata]\n
```

The path always runs from after the `a/`/`b/` prefix up to the first TAB character (or end-of-line if no TAB is present). Splitting on TAB preserves leading and internal spaces in the path while stripping the trailing-tab-plus-metadata that git emits for whitespace-containing paths.

(Secondary concern: git's `core.quotePath` C-escapes some paths. Not in scope for this fix unless trivial to address; if not, leave a comment noting the limitation.)

## Approach

### Producer-side prefix pin (prerequisite; ruthless's load-bearing item)

Before the parser can rely on `a/` and `b/` prefixes, the producer has to emit them. Today `git_io.rs:166` (`diff_two_blobs`) and `:172` (`show_commit`) invoke git without `--src-prefix=a/ --dst-prefix=b/`. If the user has `diff.mnemonicPrefix=true`, `diff.noprefix=true`, or custom `diff.srcPrefix`/`dstPrefix` in their git config, every row of the source-priority table below silently fails — even the new parser falls through to the last-resort `diff --git` rule because `+++ b/<path>` doesn't match `+++ w/<path>`.

So this plan adds, as a prerequisite step:

1. `git_io.rs:166` (`diff_two_blobs`): args become `["diff", "--no-color", "--no-ext-diff", "--src-prefix=a/", "--dst-prefix=b/", &from_spec, &to_spec]`.
2. `git_io.rs:172` (`show_commit`): args become `["show", "--no-color", "--src-prefix=a/", "--dst-prefix=b/", sha.as_str()]`.
3. Add a `parse_diff` precondition / debug-assert: if a `diff --git` line is reached without `a/` / `b/` after a producer-side change, treat it as a programmer error (or log a clear "producer contract violated" warning) rather than silently mis-parsing.
4. Confirm the change in `git_io.rs` is the COMPLETE set: grep for any other `parse_diff` callers (e.g. there may be more than `diff_two_blobs` + `show_commit`) and add the prefix flags to each.

This locks the producer→parser contract instead of having the parser guess.

### Path-source priority (parser side)

The path for each file section is gathered from up to four possible sources, in this priority order, so each git-diff shape produces a correct path:

| Source line                                       | When it fires                  | Reliable for spaces?         | FileDiff field set                                  |
|---------------------------------------------------|--------------------------------|------------------------------|-----------------------------------------------------|
| `+++ b/<path>[\t metadata]` (path ends at tab/EOL)| normal, addition               | yes                          | `path` (always); also old_path for additions = None |
| `--- a/<path>[\t metadata]` (path ends at tab/EOL)| normal, deletion               | yes                          | `old_path` (Some); for deletions, `path` from this  |
| `rename to <path>` / `rename from <path>`         | pure renames (no ---/+++)      | yes                          | `path` (rename to) + `old_path` (rename from)       |
| `Binary files a/<old> and b/<new> differ`         | binary files (no ---/+++)      | only if path lacks " and "   | `old_path` (left of ` and `) + `path` (right)       |

Field-mapping per diff shape (verbatim, no implementer guessing):

- **Modified**: `path = +++ b/<path>`, `old_path = Some(--- a/<path>)`. Equal for non-renames.
- **Added** (`--- /dev/null`): `path = +++ b/<path>`, `old_path = None`.
- **Deleted** (`+++ /dev/null`): `path = --- a/<path>`, `old_path = Some(--- a/<path>)`. (Verify against current `FileDiffMode::Removed` downstream usage; keep behavior.)
- **Renamed (pure)**: `path = rename to`, `old_path = Some(rename from)`. No `---`/`+++` present.
- **Renamed (with content)**: `path = +++ b/<path>`, `old_path = Some(--- a/<path>)`. The `rename from`/`rename to` lines still fire but `---`/`+++` take precedence.
- **Binary**: `path = right-of-' and '`, `old_path = Some(left-of-' and ')` from the `Binary files a/<old> and b/<new> differ` line.

Implementation steps:

1. Restructure `parse_diff` so the canonical path for a section is set when we see `+++ b/<path>` (preferred) or `--- a/<path>` (for deletions or as fallback). Take the substring after the `a/` / `b/` prefix up to the first TAB character, or end-of-line if no TAB is present. Do NOT trim trailing whitespace — internal/leading spaces are part of the path, only the TAB delimiter and anything after it are metadata.
2. `diff --git` becomes a boundary detector only — it starts a new `FileDiff` but does NOT set the path. The `parse_diff_git` function (or the inlined prefix check) returns `Option<()>` rather than `Option<(String, String)>`.
3. Renames are unchanged from today: the existing `rename from` / `rename to` handlers (lines 50-58) already source paths from those lines unambiguously. Pure renames (similarity 100%, no `---`/`+++`) keep working via that path; renames with content changes will get the path from `+++`/`---` as for any modified file.
4. Binary files: parse the `Binary files a/<old> and b/<new> differ` line for the path. The ` and ` separator is the only reliable split, with the known limitation that a path containing the literal substring ` and ` will mis-parse — flag this in a code comment and a TODO.
5. **No best-effort fallback to `parse_diff_git`.** The function exists today to extract the path from the `diff --git` line; this plan retires that. If a file section reaches the end with no canonical path source (no `+++`, no `---`, no rename lines, no binary line), that is a producer contract violation — log a warning, drop the file, do not silently emit a wrong path. With the prefix pin from the prerequisite step above, this case shouldn't fire for any valid `git diff` output; if it does, that's a bug we want to see, not hide.

   (If extra-defensive recovery is requested in review: recover ONLY when the `a/`-side substring on `diff --git` and the `b/`-side substring are PROVABLY identical — i.e. no whitespace ambiguity possible. Otherwise drop.)

## Tests

In `diff_parser.rs`:

- Path with a single space (`foo bar.txt`) — modified file. Test fixture must include the literal trailing TAB that git emits in the `--- a/foo bar.txt\t` / `+++ b/foo bar.txt\t` headers.
- Path with multiple spaces and a leading space (`  foo  bar.txt`) — modified file, with trailing-TAB metadata in the fixture.
- Path with a trailing-TAB followed by literal timestamp metadata (`--- a/foo bar.txt\t2024-01-01 12:00:00.000000000 +0000`) — verify the metadata is stripped and only the path remains.
- **Path with the literal substring ` b/`** (e.g. `my a/x b/y.txt`) — guards against a future "fix" that tries midpoint-splitting the `diff --git` header.
- **Real-fixture from actual `git diff` output**: capture the bytes from `git diff` on a scratch repo with a file containing spaces, store as a `tests/fixtures/diff-with-spaces.patch`, and assert `parse_diff` produces the expected `FileDiff`. Hand-rolled fixtures inevitably agree with the parser; real output catches format drift across git versions.
- **Mode-only change** (`diff --git a/foo b/foo` + `old mode 100644` + `new mode 100755`, no hunks) — confirm a `FileDiff` is produced with `mode: Modified`, `additions: 0`, `deletions: 0`, and the right path. (Today's parser likely produces this correctly via the `diff --git`-line path; verify the new parser preserves it via `---`/`+++` lines if present, or skip-if-absent and document.)
- **Prefix-pin enforcement**: assert that `diff_two_blobs` and `show_commit` invoke git with `--src-prefix=a/ --dst-prefix=b/`. Mock or inspect the constructed argv.
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
