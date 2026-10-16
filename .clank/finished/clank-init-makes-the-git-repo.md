# clank-init-makes-the-git-repo
# `clank init` outside any git repository initializes one in that directory, then sets clank up in it

## Why

The user, 2026-10-17: "clank init should init the git repo too if there isn't one."

Today, in a directory with no git repository above it, `clank init` stops at `resolve_cwd_repo` with "`<dir>` is not inside a git repository". The user has to run `git init` and then repeat the command.

## What to build

- **Where.** `init::run` resolves the repo first. When resolution fails only because no git repository contains the directory, it initializes one, then continues with the normal init.
  - Without `--repo`, the directory is the cwd.
  - With `--repo <dir>`, it is that directory, when no repository contains it.
- **What is left alone.** A directory already inside a repository, even a parent one, is not touched: the existing discovery behaviour wins. So is a resolution failure for any other reason, such as a missing directory or a canonicalization error.
- **Through the layer.** The init is a new mutation in `git_plumbing`, backed by gix (`gix::init`) if it can create a normal non-bare repository with the user's default initial branch and config. Otherwise it is a subprocess, with a one-line "why not gix" justification. `git_boundary` must stay green.
- **Output.** Init prints that it created the repository, with its path, before the existing output.
- **Nothing else.** No commit is made and no remote is added.

## Acceptance

Tests call the init core, not the binary.

- A temp directory with no repository above it ends up with a working repository whose work tree is that directory, and the usual clank scaffold (`.clank/.gitignore`, the post-rewrite hook) is in place.
- A temp directory inside an existing repository is not re-initialized; the parent repository is used, as today.
- A missing directory still fails, with no repository created.
- `git_boundary` passes.

## As built

- **`git_io::discover(start) -> Discovery`**: `WorkTree(root)`, `Bare(git_dir)`, `Absent` or `Failed(why)`.
  - `Absent` requires gix's `NoGitRepository` and also no git metadata on the path (`git_metadata_on_path`). For `start` and every ancestor, that means no `.git` entry, and no git dir. A git dir is three of `HEAD`, `objects`, `refs` and `config`, so a repository missing any single part still counts, while a single stray file of those names does not. The probe never reads file contents: a `config` with a `[core]` section is ordinary INI, not git evidence. gix reports "no repository" past any candidate it cannot use. Tests caught three cases: a broken `gitdir:` file (in implementation), and a bare repository missing `refs`, then one missing `HEAD` (in codex's reviews).
  - The path probe treats only not-found as absence; any other I/O error is `Failed`.
  - Every other discovery error is `Failed`.
  - The shared `discover_work_dir` and `resolve_repo` are unchanged.
- **`git_plumbing::init_repo(dir)`** runs `git init --quiet`, as a subprocess. Why not gix: `git init` applies the user's `init.*` config (default branch) and template directory, which gix's init does not. No commit, no remote.
- **`init::repo_for_init(explicit, cwd)`**, init only, called by `init::run`:
  - it canonicalizes the selected directory (`--repo`, else the cwd) and requires it to be a directory;
  - a work tree, found upward and including a linked worktree, is used as is;
  - `Absent` initializes the repository and prints "created a git repository in …";
  - `Bare` and `Failed` refuse, saying nothing was created.
- **Tests** (`cli::init`):
  - cwd and explicit `--repo` outside any repo each create a repo whose top level is that directory, with no `HEAD` commit and no remote, and the scaffold and post-rewrite hook install into it;
  - a subdirectory resolves upward to its repo without creating `.git` there;
  - a linked worktree stays a linked worktree;
  - a missing path, a broken `.git` file, a bare repository missing any one of `HEAD`, `refs`, `objects` or `config`, or one without `HEAD` whose config was emptied, creates nothing: no `.git` and no `.clank`;
  - an ordinary project containing a `config` file with a `[core]` section is initialized, and the file is left byte-identical;
  - in `git_io`, a path entry that cannot be inspected (an injected permission error) is unknown, not absent;
  - a bare repository is refused.
  - The branch name comes from the user's own `git init` configuration, by construction; it is not asserted, because setting git config for a test means changing process-wide environment.
- **Mutations, each caught:**
  - no `.git`-on-path check;
  - no bare-shape check;
  - one part being enough;
  - requiring all four parts;
  - probe errors read as absence;
  - `Absent` without the init;
  - a bare repository accepted.
- **`git_boundary`** still fails only on the two pre-existing sites (`stop_hook.rs` and `grok_hook_tests.rs`), which this change does not touch.
