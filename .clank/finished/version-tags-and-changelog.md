# version-tags-and-changelog
# Releases are tagged v<semver>, with a CHANGELOG

## Why

RELEASE-CHECKLIST.md §1 `version-tags-and-changelog`. All crates are at
`0.0.1` with no tags and no CHANGELOG. `clank --version` prints
`0.0.1+<sha>`, and prints the bare version only for a clean checkout at a
tag (`build_stamp_inputs.rs`). The release workflow needs a defined tag
form to check against.

## What changes

- Tag form: `v<semver>`. The first release is `v0.1.0`.
- Every workspace crate takes its version from `[workspace.package]`
  version, set to `0.1.0`. Inter-crate path dependencies carry
  `version = "0.1.0"` as crates.io requires; that part is shared with
  `crates-io-packaging`.
- `CHANGELOG.md` in Keep a Changelog form. `0.1.0` summarises what clank
  is, not a commit list, and an `Unreleased` section sits above it.
- `scripts/release-check.sh`, used by the release workflow:
  1. builds;
  2. runs `clank --version`;
  3. requires exactly `clank <tag without v>`, with no `+sha` and no
     `.dirty`;
  4. requires a `CHANGELOG.md` section for that version.
- RELEASE-CHECKLIST.md marks the item done.

## Acceptance

- In a test fixture, `human_version` gives the bare version for a
  `v0.1.0` tag at a clean HEAD, and `+sha` otherwise. The existing build
  stamp tests are extended for the `v` prefix.
- `release-check.sh` exits non-zero on a mismatched tag, on a dirty
  build, and on a missing changelog section. Each case is tested in the
  `it` harness with fixture inputs; nothing is published.

## As landed

- **06348c88:**
  - `[workspace.package]` sets version `0.1.0` for all five crates, and
    the inter-crate path dependencies carry `version = "0.1.0"`.
  - `CHANGELOG.md` is in Keep a Changelog form.
  - `scripts/release-check.sh` was added, along with
    `it/release_check.rs`. The build-stamp tests use `0.1.0` and
    `v0.1.0`.
- **0017f4b6, review fixes:**
  - The script takes the executable from
    `cargo build --message-format=json`. It no longer guesses
    `target/release`.
  - It accepts only a plain `vMAJOR.MINOR.PATCH` tag, which must equal
    the version the `clank` crate declares.
  - The tests build a fixture crate to exercise the default path, with
    a stale binary at the guessed location, and cover malformed tags and
    tags with a development suffix.
  - The README's version example no longer names a version.
- **Mutation checks:** the release check's prefix match, missing
  changelog check, guessed path, loose tag regex and missing
  declared-version check each fail a named test.
