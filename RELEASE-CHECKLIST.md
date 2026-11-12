# Release checklist

Current preparation: **v0.2.0**, 2026-11-12. This is the first published release;
0.1.0 was a development version, not a published tag. Earlier planning details
and implementation evidence remain in `.clank/finished/`.

## Release policy

- Publish prebuilt binaries on GitHub Releases; serve the checksum-verifying
  installer from GitHub Pages at `llfourn.github.io/clank/install.sh`.
- Ship macOS and Linux, arm64 and x86_64, with both `clank` and its sibling
  `clank-web-cloudflare`. Linux uses static musl and requires Linux 5.3 or newer.
- No Windows, crates.io or Homebrew release. The crates.io name belongs to an
  unrelated project; installing this checkout through Cargo remains supported.
- Licence: 0BSD. macOS binaries are unsigned and not notarized; README installation
  guidance explains the Gatekeeper distinction for browser downloads.
- Publish a plain `vMAJOR.MINOR.PATCH` tag once, at the exact reviewed commit whose
  CI passed. Never move a published version tag to repair a failed release.

## Implemented release infrastructure

- [x] **Licence and attribution.** Workspace licence/repository metadata,
  `LICENSE`, `THIRD-PARTY-NOTICES.md` and generated Rust dependency notices ship
  with the binaries. CI checks licences and notice freshness.
- [x] **Linux CI.** Pushes and pull requests run formatting, strict Clippy,
  fixture-based tests, maintenance tests, workflow linting and licence checks.
  Each test run isolates its runtime and clears inherited session identities.
  A separate job checks the release validator from a production-only Cargo cache.
- [x] **Versions and changelog.** Workspace and internal dependencies use 0.2.0.
  `scripts/release-check.sh` requires matching tag, Cargo version, binary version
  and changelog section. The release notes come from `CHANGELOG.md`.
- [x] **Durable configuration and upgrades.** Versioned config boundaries reject
  malformed or unsupported documents instead of silently applying defaults.
  Monitors and other long-lived processes retire on build replacement. Repository
  blocks migrate legacy state with occurrence and delivery evidence preserved.
- [x] **Post-rewrite hooks.** Init and doctor share Git's effective hook location;
  plain hooks can be chained, managed/symlinked hooks are left alone.
- [x] **Security work.** `SECURITY.md` documents the threat model and residual
  risks. Remote tokens use the macOS keychain or a private Linux file, with
  conflict-aware migration; the remote remains opt-in. Same-user processes are
  not a security boundary, and external event text remains untrusted input.
- [x] **Website and reference.** The Pages workflow publishes `book/`, including
  the installer. Generated CLI reference freshness is checked by tests.
- [x] **Release binaries and installer.** The Release workflow builds and
  smoke-tests the four targets natively, checks versions, packages both binaries,
  requires the complete archive set, creates `SHA256SUMS`, and publishes last.
  Installer fixtures cover platform selection, checksums and custom destinations.
- [x] **Runtime exercised during development.** Event-driven local monitoring,
  attended background work, master-following event sources, repo-wide blocks and
  test-fixture cleanup have passed their plan reviews. This is development and
  automated-test evidence, not a substitute for the clean-account walks below.

## Verification not claimed

- [ ] **Clean first run per harness.** The procedure is
  `docs/first-run-walk.md`; results live in `docs/harness-support.toml`.
  No clean-account walk or complete native acceptance set is recorded as passed
  for Claude Code, Codex, Grok or OpenCode. OpenCode has no native acceptance test
  yet. The README therefore says "not yet tested" for each harness.
- [ ] **Full macOS CI matrix.** Routine automated CI runs on Linux. macOS is used
  for development and local checks; release runners build and smoke-test the
  executables, rather than running the full suite on all four targets.
- [ ] **macOS signing/notarization.** Not provided for this release.
- [ ] **Real keychain reinstall prompt behavior.** Tests use isolated stores;
  whether macOS asks again after a reinstall remains unverified.

Ignored live tunnel and installed-harness tests are reported as not run, never
counted as passes. These limitations remain documented in the release notes and
support record; they do not become successful checks merely because a tag exists.

## v0.2.0 publication run

These operations follow review/finalization of the preparation commit. Record
actual results in the release handoff, rather than checking them off in advance.

- [ ] Push the reviewed commit to `origin/master` without rewriting history.
- [ ] Require successful CI for that exact commit before pushing `v0.2.0`.
- [ ] Confirm the Release workflow built all four archives and published the
  release with the expected changelog notes and `SHA256SUMS`.
- [ ] Verify asset checksums and exercise the v0.2.0 installer with a temporary
  destination, leaving the developer's installed binaries untouched.
- [ ] Report the GitHub release URL and the observed build/install results.

## Follow-up work

- Record real clean-account harness walks and native acceptance results.
- Add native OpenCode acceptance coverage and, if practical, a scheduled harness
  acceptance job with pinned versions.
- Revisit macOS signing/notarization and additional routine platform CI.
