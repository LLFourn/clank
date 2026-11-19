# release-clank-v0-3-0
# Release clank v0.3.0

## Why

The user asked for a v0.3.0 release, driven through to publication. v0.2.0 is the
only published tag. Since then 35 plans have landed on local master, none of them
pushed, and they include breaking changes: one state file per agent with no
migration, monitor protocol 25, no more block and inbox migrations, and block
answers deleted once delivered. CHANGELOG.md's `[Unreleased]` section has only its
Breaking entries and a "missing from the 0.2.0 notes" section. Nothing user-visible
since v0.2.0 is listed as added, changed or fixed.

## Committable preparation (one commit)

- Set every workspace crate and inter-crate dependency to 0.3.0. Update the lockfile
  without unrelated dependency upgrades.
- Turn `[Unreleased]` into `[0.3.0]` with today's date and leave an empty
  `[Unreleased]` above it. Write accurate release notes from `git log v0.2.0..HEAD`
  and the matching `.clank/finished/` plans, grouped the way 0.2.0's are: Breaking,
  Added, Changed, Fixed, and Upgrading. Describe user-visible behaviour, not the
  internals. Notable items include:
  - `clank remote link`
  - repo-qualified agent pane titles
  - event-driven recovery and monitor reach in place of polling
  - wall-clock poll times
  - harness adapters
  - Codex background-work wakes
  - only the master raises blocks
  - reopen pane

  Check each Breaking entry against the code as it now stands. For example, the
  protocol number must match the constant. Keep the "missing from 0.2.0" section.
- Upgrading section: say plainly that after upgrading every agent is unbound and
  must be restarted, or rebound with `clank as`. Say that monitors and TUIs of the
  older build must be restarted.
- Update RELEASE-CHECKLIST.md for v0.3.0. Its current-preparation line and
  publication run should name 0.3.0. Keep the unclaimed-verification items unclaimed.
  Change version pins in README and the book if they name 0.2.0.
- Validate formatting, strict Clippy, and the release, installer, licence,
  reference and harness-support tests. Run `scripts/release-check.sh v0.3.0` against
  a locally built binary, and the maintenance tests. Also check that the dependency
  notices and the generated CLI reference are fresh.

Publishing and remote builds come after this review. They are not a reason to
withhold FINISHED from preparation that is complete and correct.

## Authorized release operations after finalization

The user's request authorizes pushing, merging and tagging. No further approval is
needed.

- Push master to a release branch and open a PR to master. Wait for CI to pass on
  it, then land it with `gh pr merge --rebase`, never a merge commit. Move local
  master onto origin/master.
- Wait for CI to pass on that exact origin/master commit. Then create and push the
  annotated `v0.3.0` tag once at that commit. Never move a published tag.
- Follow the Release workflow to completion. Verify the four archives,
  `SHA256SUMS` and the release notes. Run the installer against a temporary
  destination without touching the installed binaries, and report the release URL.
