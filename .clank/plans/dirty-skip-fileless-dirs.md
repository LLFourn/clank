# dirty-skip-fileless-dirs

## Problem

`clank status` reports a worktree as dirty with phantom "untracked"
entries that `git status` considers clean. Observed in a linked
worktree (`frostsnap/.clank/worktrees/full-app-sim-driver`):

- `git status --porcelain` → empty (clean).
- `clank status` → `dirty: yes (3 untracked)`.

The three "untracked" paths are tool-generated scaffolding directories:

```
frostsnapp/macos/Runner.xcworkspace/xcshareddata/swiftpm
frostsnapp/macos/Runner.xcodeproj/project.xcworkspace/xcshareddata/swiftpm
frostsnapp/android/.kotlin
```

`git check-ignore` says they are NOT ignored. Each is a directory whose
entire subtree contains **zero files** — only further empty
subdirectories (`swiftpm/configuration/`, `.kotlin/sessions/`).

## Root cause

Git tracks files, not directories: a directory tree with no files in it
is invisible to `git status` (its default `-unormal` collapses an
untracked directory to a single `dir/` entry **only when the directory
contains at least one file**). A directory containing only empty
subdirectories yields no untracked output at all.

clank's gix-backed `status_walk` (`crates/cli/src/git_io.rs`) uses the
gix dirwalk in its default `CollapseDirectory` untracked mode. gix's
`emit_empty_directories` is already `false`, so a *literally* empty leaf
directory is suppressed — but `swiftpm/` is not literally empty (it has
a child `configuration/`). gix therefore collapses `swiftpm/` into one
untracked entry because all of its children are untracked, even though
no actual file exists anywhere beneath it. clank counts that collapsed
directory as untracked → the dirty/untracked count diverges from git.

This is a pure read-side divergence from git semantics; it affects every
caller of `working_tree_status` / `working_tree_dirty*` (the status TUI's
dirty badge, the `unfinish` clean-worktree guard, rewrite policy, etc.).

## Fix

Make `status_walk`'s untracked set match git's porcelain semantics: an
untracked **directory** is reported only if it contains at least one
file (recursively). Inside the layer (`git_io.rs`), in the
`Summary::Added` arm, drop an entry whose on-disk kind is a directory
that contains no files.

- gix exposes the entry's `disk_kind` (`gix-dir` `entry::Kind::{File,
  Directory,Repository,Symlink}`); use it to identify directory entries
  rather than re-statting.
- For a directory entry, walk it and keep it iff a file is found, with
  **early-exit on the first file** (so a large untracked dir like
  `node_modules/` costs one file probe, matching the work git does to
  decide it is non-empty). A `Repository` (nested git) entry is content
  git reports — keep it.
- File and symlink entries are unchanged.

Keep the existing `CollapseDirectory` granularity (clank should keep
reporting `dir/` collapsed, like `git status` default — not expand to
every file).

## Verify

Fixture-based unit test in `git_io.rs` tests (test code may `git init`
a temp repo; in-process library call, no binary spawn):

- A worktree containing only an empty nested directory tree (`a/b/`,
  no files) → `working_tree_status().untracked` is empty AND
  `git status --porcelain` on the same fixture is empty (assert they
  agree).
- Positive control: a directory with one file (`c/d.txt`) → untracked
  contains the collapsed `c/` entry (unchanged behavior).
- Tracked-change detection is unaffected (a modified tracked file still
  shows in `changed`).

## Out of scope

- The global-excludes / per-worktree `info/exclude` loading paths (gix
  read those correctly here — verified the rules that DID apply came
  from the common-dir `info/exclude`). This plan is only the
  fileless-directory divergence.
- Any change to collapse granularity or to the `changed` (tracked) set.
