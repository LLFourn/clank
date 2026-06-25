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
entry that is **not itself a directory** (recursively). Git surfaces a
directory as untracked when it contains content — and to git, content is
a regular file, a symlink, OR an exotic node (fifo/socket/device); only
a tree of pure subdirectories is "nothing". Inside the layer
(`git_io.rs`), in the `Summary::Added` arm, drop an entry whose on-disk
kind is a directory with no non-directory descendant.

- gix exposes the entry's `disk_kind` (`gix-dir` `entry::Kind::{File,
  Symlink,Directory,Repository,Untrackable}`); use it to classify the
  entry rather than re-statting.
- **Only `disk_kind == Some(Directory)` triggers the re-walk.** `File`,
  `Symlink`, `Repository` (nested git — content git reports), the
  `Untrackable` exotic kinds, and a defensive `None` are all KEPT
  unchanged.
- The re-walk keeps the directory iff it finds **any non-directory
  entry** (regular file, symlink, fifo/socket/device). Decide purely by
  `std::fs::DirEntry::file_type()` — **never follow symlinks**: a
  symlink (even one pointing at a directory) counts as a kept
  non-directory entry and is NOT traversed, so the walk cannot cycle or
  escape the worktree. Recurse only into entries whose `file_type()` is
  a real directory.
- At each level, check all non-directory entries BEFORE recursing into
  subdirectories, so a directory that does have content exits early. The
  genuinely fileless case (the bug) has no early exit — it must exhaust
  the collapsed subtree to conclude "empty" — but cost is bounded by the
  untracked set, which is fine for this display path.
- `status_walk` holds only `git: &gix::Repository`; build the absolute
  path to re-walk from `git.workdir()` joined with the entry's
  rela_path.

Keep the existing `CollapseDirectory` granularity (clank should keep
reporting `dir/` collapsed, like `git status` default — not expand to
every file).

## Verify

Fixture-based unit tests in `git_io.rs` tests (test code may `git init`
a temp repo; in-process library call, no binary spawn). Each fixture
asserts clank's untracked set AGREES with `git status --porcelain` on
the same tree:

- **Fileless tree (the bug):** only an empty nested directory tree
  (`a/b/`, no files) → `working_tree_status().untracked` is empty, and
  `git status --porcelain` is empty too.
- **Symlink-only dir (guards the corrected predicate):** a directory
  whose only descendant is a SYMLINK and contains no regular file
  anywhere (`e/link -> somewhere`) → it MUST stay in `untracked`, and
  `git status --porcelain` lists it too. (A naive "keep iff a file is
  found" predicate would wrongly drop this — this fixture is what
  catches that re-divergence.)
- **Positive control:** a directory with one regular file (`c/d.txt`) →
  untracked contains the collapsed `c/` entry (unchanged behavior).
- **Tracked changes unaffected:** a modified tracked file still shows in
  `changed`.

## Out of scope

- The global-excludes / per-worktree `info/exclude` loading paths (gix
  read those correctly here — verified the rules that DID apply came
  from the common-dir `info/exclude`). This plan is only the
  fileless-directory divergence.
- Any change to collapse granularity or to the `changed` (tracked) set.
