# release-clank-v0-2-0
# Release clank v0.2.0

## Why

Lloyd asked to publish clank on GitHub as v0.2.0 after exercising the recent
runtime, test, event-routing and repository-block changes. GitHub currently has
no releases or version tags; Cargo still names the development version 0.1.0.

## Committable preparation

- Set every workspace crate and inter-crate dependency to 0.2.0; update the
  lockfile without unrelated dependency upgrades.
- Write accurate v0.2.0 release notes in CHANGELOG.md. This is the first published
  release, not an upgrade from a published v0.1.0. Cover the existing team/review
  workflow and recent event-driven runtime, background-work tracking, test
  reliability, master-following events and repository-owned blocks. Explain the
  block CLI change for development users.
- Update release pin examples and reconcile the checklist's stale release status
  with implemented infrastructure. Do not claim unrun native harness acceptance
  tests, signing/notarization, or other unavailable verification succeeded.
- Validate formatting, strict Clippy, focused release/installer/packaging gates,
  maintenance tooling, generated CLI reference and dependency notice freshness.
  Reuse the just-completed independent full-suite validation for unchanged code.

Implement preparation in one complete commit. The review gate assesses these
committable artifacts; publishing and remote builds are subsequent operations,
not a reason to withhold FINISHED from complete, correct preparation.

## Authorized release operations after review/finalization

- Push the reviewed release commit to origin/master without rewriting history.
  Require successful CI for that exact commit before publishing the version tag.
- Create and push the annotated v0.2.0 tag once, pointing at that tested commit.
  The existing Release workflow builds clank plus clank-web-cloudflare natively
  for macOS arm64/x86_64 and Linux musl arm64/x86_64, verifies versions and creates
  archives/checksums before publishing. Never move a published tag to fix a build.
- Follow the workflow to completion, resolve failures within the release scope,
  and verify the published tag, four archives, checksum manifest and release
  notes. Exercise the installer against a temporary destination and the new
  version on this machine. Report the release URL and any actual limitations.
- No crates.io or Homebrew publication; these are not chosen release channels.

The user's release request authorizes pushing the branch/tag and publishing the
GitHub release. No additional human approval is needed for those steps.

## Preparation validation

All five workspace packages and their versioned internal dependencies now use
0.2.0; the lockfile changes only those five package versions. Release notes
identify this as the first published release and distinguish remaining native
harness verification from automated checks.

Passed locally: formatting, strict workspace/all-target Clippy, 41 focused
release/installer/licence/reference/harness-support tests, five maintenance
tests, cargo-deny licence checks, full dependency-notice freshness, locked
offline metadata/version consistency, release-note extraction and diff checks.
The immediately preceding repository-block plan's full-suite evidence covers
unchanged runtime code. Remote CI, tag publication, four native release builds
and the actual downloaded installer smoke test remain subsequent operations.
