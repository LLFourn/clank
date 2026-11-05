# release-binaries-and-installer
# A tag builds macOS and Linux binaries, attaches them to a GitHub release, and an installer fetches them

## Why

RELEASE-CHECKLIST.md §2 `release-binaries-and-installer`. Lloyd chose
(§0, 2026-11-05):
- prebuilt binaries for macOS and Linux, arm64 and x86_64;
- published on GitHub Releases, which is where the website's installer
  fetches them (`website-on-github-pages`);
- a `curl | sh` installer.

The TUI runs `clank-web-cloudflare` as a sibling process, so both
binaries ship together.

## What changes

- `.github/workflows/release.yml`, triggered by a `v*` tag:
  - a build matrix:
    - `aarch64-apple-darwin` and `x86_64-apple-darwin` on macOS runners;
    - `x86_64-unknown-linux-gnu` and `aarch64-unknown-linux-gnu` on Linux
      runners, the arm64 one native or cross-compiled. musl is
      considered: static Linux binaries avoid glibc floors, and the plan
      records the choice;
  - each target builds `clank` and `clank-web-cloudflare` in release
    mode and runs `scripts/release-check.sh` (version equals tag) and a
    smoke `clank --help`;
  - archives named `clank-<version>-<target>.tar.gz`, each containing
    both binaries, LICENSE and THIRD-PARTY-NOTICES.md, plus a
    `SHA256SUMS` file;
  - a GitHub Release for the tag, with the archives and checksums and
    the CHANGELOG section as notes;
- `book/install.sh`, served by the website:
  - detects OS and arch, downloads the matching archive from
    `https://github.com/LLFourn/clank/releases/latest/download/` (or
    `/releases/download/v$CLANK_VERSION/` when that is set), and
    verifies it against `SHA256SUMS`;
  - installs both binaries to `~/.local/bin` (or `$CLANK_INSTALL_DIR`)
    and says if that directory is not on `PATH`;
  - is POSIX sh.
- **macOS signing:** none for 0.1.0. A binary fetched with curl isn't
  quarantined, so Gatekeeper does not block it. The README says to
  install with the script or cargo, not a browser download. Notarization
  is a later item.

## Acceptance

- The workflow passes `actionlint` (in CI) and an `it` gate asserts the
  matrix targets and the version check step.
- `install.sh` is tested with shellcheck, and with a local fixture
  server serving fake archives:
  - right target picked;
  - checksum mismatch refused;
  - install dir honoured.
- A dry run of the workflow on a throwaway `v0.0.0-test` tag in a branch
  is not part of acceptance. Real releases are cut on Lloyd's go.

## As landed

- **Implementation** (36310f01 after rebase):
  - **`release.yml`.**
    - A matrix runs on native runners: `macos-15` (aarch64-apple-darwin),
      `macos-15-intel` (x86_64-apple-darwin), `ubuntu-24.04`
      (x86_64-unknown-linux-musl) and `ubuntu-24.04-arm`
      (aarch64-unknown-linux-musl).
    - **The Linux targets are static musl, replacing the planned `*-gnu`
      ones,** so there is no glibc floor. The kernel floor is Linux 5.3,
      from `pidfd_open`.
    - Each leg runs `release-check.sh` and both binaries. It packs
      `clank-<v>-<t>.tar.gz` with LICENSE, `THIRD-PARTY-NOTICES.md` and
      `rust-dependencies.txt`.
    - One `publish` job (`needs: build`, and the only one with
      `contents: write`) requires exactly four archives, writes
      SHA256SUMS, and creates the release last with the changelog
      section.
  - **`book/install.sh`.**
    - It resolves "latest" once through the releases API, or uses
      `CLANK_VERSION`.
    - It verifies the checksum before installing, then stages both
      binaries and renames them into place.
  - **Gates.**
    - `release_workflow` checks the structure and has 8 self-test
      mutations.
    - `install_script` runs over a local fixture server and covers all
      four platforms, pinned versus latest, checksum refusal with an
      existing install left untouched, and a custom directory.
    - shellcheck covers the installer and the release scripts.
    - CI gains an actionlint step (v1.7.12) and installs shellcheck.
- **f6debe3e:**
  - `pages.yml`'s validation step is now a block scalar. The unquoted
    `cli_reference::` had made the file invalid YAML, which is why the
    first Pages run failed at startup.
  - The README states the real Linux floor.
- **Known gaps:**
  - The `chmod 755` mutant survives under the normal umask.
  - actionlint runs with `-ignore SC2086`, which the intentional
    `unset $SESSION_IDENTITY_VARS` split needs.
  - The musl arm build, the `macos-15-intel` label and the real release
    flow are proven only on GitHub Actions and on the first tag.
