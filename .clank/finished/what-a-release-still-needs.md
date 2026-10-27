# what-a-release-still-needs
# What a release still needs

## Problem

Lloyd wants to cut real releases soon. `RELEASE-CHECKLIST.md` (144 lines, last
touched 2026-10-20) is meant to list what has to happen first, but a lot has
landed since it was written. It may now mark things done that aren't, list things
that no longer apply (it names commands and mechanisms that may since have been
renamed or deleted), and leave out things that matter. A stale checklist is worse
than none: a release cut from it would ship known gaps.

## Deliverable

This plan's only deliverable is a document: an up-to-date `RELEASE-CHECKLIST.md`.
There is no code. Go through every item:

1. **Check each existing item against the code as it stands now.** Mark each one
   done (cite the commit or file that did it), still needed (rewrite its problem
   and fix to match today's code), or obsolete (delete it with a one-line reason
   in the commit message, not in the file). Check that the commands and mechanisms
   it names still exist, using `clank --help` and the source.
2. **Find what's missing.** At minimum, look at:
   - **Versioning:** the version scheme, tags, `clank --version` (the plan
     `the-tui-pane-names-the-clank-version` sets up the version string),
     CHANGELOG, and what a release build reports.
   - **Packaging:** crate metadata (license, description, repository; `publish =
     false` on internal crates), whether crates.io or binary releases or both,
     `--locked` installs, and platforms (macOS/Linux; is Windows a non-goal?).
   - **CI:** is there any `.github/workflows`? What gates a release (fmt, clippy,
     the `it` harness, the source-scanning gates)? Do tests that need node or
     zellij skip cleanly?
   - **Upgrades:** what happens to a user's existing `.clank/` dirs, global config,
     installed hooks and running monitors when they install a new release.
     Durable formats need migrations, and the upgrade path should not break
     sessions that are already running.
   - **First run:** `clank setup` → `clank init` → `clank open` on a clean
     machine, plus how each supported harness (claude, codex, opencode, grok)
     behaves on its current release.
   - **Docs:** README and `book/` accuracy against the current CLI; stray
     top-level files (`GROK-INTEGRATION.md`, `cli-audit.md`, `open-worktree.sh`)
     to fold in or delete.
   - **Security:** what secrets can reach logs or agent context (web bridge,
     tunnel tokens); default network exposure; hooks that run user commands.
   - **Licensing and attribution:** LICENSE file, dependency licences.
3. **Keep every unchecked item queueable,** as the file already asks: a plan
   name, the problem, and the shape of the fix. Order the items so that
   release-blocking ones come first, and mark which are blockers and which can
   wait until after the first release.

## Out of scope

- Fixing any of the items. Each becomes its own plan later.
- Release tooling itself.

## Acceptance

- Every item in the old checklist has been decided (done, still needed or
  obsolete), and the commit message lists the obsolete ones with their reasons.
- Every claim of "done" or "missing" was checked against the source or the CLI,
  not remembered.
- Blockers are separated from items that can wait until after the first release.
