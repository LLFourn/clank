#!/usr/bin/env bash
# Check that a release build matches its tag before anything is published.
# Usage: release-check.sh <tag> [clank-binary]
# Without a binary, builds clank in release mode and checks the executable
# Cargo reports building.
set -euo pipefail

tag="${1:?usage: release-check.sh <tag> [clank-binary]}"
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
manifest="$root/Cargo.toml"

# Plain MAJOR.MINOR.PATCH only: no pre-release or build suffix, so a
# development stamp like `+<sha>.dirty` can never be named as a release.
semver='^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$'
[[ "$tag" =~ $semver ]] || {
    echo "tag '$tag' is not v<MAJOR>.<MINOR>.<PATCH>" >&2
    exit 1
}
version="${tag#v}"

declared="$(cargo metadata --locked --offline --no-deps --format-version 1 --manifest-path "$manifest" |
    python3 -B -I -c 'import json, sys; print(next(p["version"] for p in json.load(sys.stdin)["packages"] if p["name"] == "clank"))')"
if [[ "$declared" != "$version" ]]; then
    echo "the tag says $version, but the clank crate declares $declared" >&2
    exit 1
fi

bin="${2:-}"
if [[ -z "$bin" ]]; then
    # Cargo's own artifact report, not a guessed path: CARGO_TARGET_DIR,
    # a configured target-dir or build target all move the executable.
    bin="$(cargo build --locked --release -p clank --bin clank --manifest-path "$manifest" --message-format=json |
        python3 -B -I -c '
import json, sys
exes = [m["executable"] for m in map(json.loads, sys.stdin)
        if m.get("reason") == "compiler-artifact" and m["target"]["name"] == "clank"
        and "bin" in m["target"]["kind"] and m.get("executable")]
print(exes[-1] if exes else "")')"
    [[ -n "$bin" ]] || {
        echo "cargo reported no clank executable" >&2
        exit 1
    }
fi

# The build stamps `+<sha>` (and `.dirty`) unless HEAD is a clean checkout
# at the tag, so an exact match proves the binary is the tagged source.
got="$("$bin" --version)"
if [[ "$got" != "clank $version" ]]; then
    echo "built '$got', but the tag says 'clank $version'" >&2
    exit 1
fi

if ! grep -qxF "## [$version]" "$root/CHANGELOG.md"; then
    echo "CHANGELOG.md has no '## [$version]' section" >&2
    exit 1
fi
echo "release $tag: version and changelog agree"
