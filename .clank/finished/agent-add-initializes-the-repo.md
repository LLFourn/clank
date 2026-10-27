# agent-add-initializes-the-repo
# agent add initializes the repo

## Problem

`clank agent add` works in a repo where `clank init` never ran. It creates `.clank/config.json` and the agent's directory, but not what init sets up: `.clank/.gitignore` and the `post-rewrite` hook that moves review feedback across rewritten commits. Observed 2026-10-27 in `~/tmp/oc-live`: after `git init` and `clank agent add qwen --tool opencode`, `clank status` reported the repo dirty with an untracked `.clank/` and no ignore file. The OpenCode agent spent its turn investigating that. Without the hook, a later `clank purge --drop`, autosquash or rebase would also lose feedback.

## What changes

Split what `clank init` does into two parts, and have `agent add` use only the first:

- **Repo support setup:** the managed `.clank/.gitignore` and plans scaffold, the existing permission migration if init still does it, the git `post-rewrite` hook (including the linked-worktree hook path init already handles), and their diagnostics. This becomes one shared function. `clank init` calls it, and so does `agent add`. The existing foreign-file rules are unchanged, and they differ by file. An unmanaged `.clank/.gitignore` is kept byte-for-byte and setup fails, as `init::write_scaffold` does today. A foreign `post-rewrite` hook is kept with the existing warning, and setup succeeds. The shared setup never force-overwrites either file.
- **Roster seeding:** importing the user's default team, and `--team`. This stays `clank init`'s alone. An explicit `agent add` in a repo with no roster keeps its current meaning: it adds only the requested agent, and the first default-role agent becomes master. No default team is imported and no extra panes appear.

Rules for `agent add` in a repo with no `.clank/config.json`:

- **Validate first:** the requested add is checked before anything is written.
- **Serialized:** the absence check, the support setup and the first roster write run under the existing roster-mutation lock, so two first adds at once both survive with exactly one master.
- **Setup before roster:** the roster is only written after the support setup has completed. If setup fails, no roster is published.
- **Honest summary:** one line saying what was actually installed, e.g. `initialized clank in /path: wrote .clank/.gitignore, installed the post-rewrite hook`. It says "kept your existing post-rewrite hook" instead when it left a foreign one alone.
- **Scope:** `agent add --global` stays a user-library operation and never touches the current repo. Other commands still require an existing git worktree; init's policy of creating a git repo doesn't spread.

Any other command that can create `.clank/config.json` from nothing either uses the same support setup or is confirmed unable to reach that state. Grep the roster and config writers.

`clank doctor` reports a repo that has a roster but is missing the managed ignore or the hook, and says to run `clank init`. It uses the existing `init_facts` classifiers and path resolution, not a second definition of a healthy repo.

## Acceptance

In-process, with real temporary git repos:

- `agent add` in a fresh `git init` repo installs exactly the support files `clank init` would, adds only the requested agent as master, and prints what was installed.
- With a user default team present, the first explicit `agent add` still adds only the requested agent, with its default master role. A same-label entry in that team causes no duplicate error.
- `agent add --global` doesn't initialize the current repo.
- Two concurrent first adds keep both agents and exactly one master.
- When support setup fails (injected), no roster is written.
- An unmanaged `.clank/.gitignore` (the real classifier path, not an injected failure): kept byte-for-byte, `agent add` fails, and no roster is written.
- A foreign `post-rewrite` hook: kept byte-for-byte with the existing warning, the add succeeds, and the summary says the hook was kept.
- `agent add` in an initialized repo changes nothing but the roster.
- Doctor flags a missing managed ignore and a missing hook independently, including the linked-worktree hook path.
- Mutation check: skipping the support setup call fails the first test; importing the default team fails the default-team test.
