#!/usr/bin/env bash
# Check a release's tag, Cargo artifact, version and changelog using the
# same Rust validators exercised by cargo test. No Python prerequisite.
# Usage: release-check.sh <tag> [clank-binary]
set -euo pipefail

tag="${1:?usage: release-check.sh <tag> [clank-binary]}"
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
# The production build does not fetch example/dev dependencies. Provision this
# tool through Cargo here; --locked fixes resolution, but fetching must remain
# allowed on a clean release runner. The validator's metadata read is offline.
exec cargo run --locked --quiet --manifest-path "$root/Cargo.toml" \
    -p clank --example maintenance -- release-check "$root" "$tag" "${@:2}"
