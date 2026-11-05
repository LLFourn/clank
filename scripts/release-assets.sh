#!/usr/bin/env bash
# Check a release's archives and write their checksums.
# Usage: release-assets.sh <version> <dir>
# <dir> must hold exactly one archive per release target and nothing
# else; writes <dir>/SHA256SUMS over them.
set -euo pipefail

usage="usage: release-assets.sh <version> <dir>"
version="${1:?$usage}"
dir="${2:?$usage}"

targets=(
    aarch64-apple-darwin
    x86_64-apple-darwin
    aarch64-unknown-linux-musl
    x86_64-unknown-linux-musl
)
want="$(for t in "${targets[@]}"; do echo "clank-$version-$t.tar.gz"; done | LC_ALL=C sort)"
got="$(cd "$dir" && find . -mindepth 1 -maxdepth 1 | sed 's|^\./||' | LC_ALL=C sort)"
if [[ "$got" != "$want" ]]; then
    echo "$dir must hold exactly the release archives" >&2
    echo "want:" >&2
    echo "$want" >&2
    echo "got:" >&2
    echo "$got" >&2
    exit 1
fi

if command -v sha256sum >/dev/null; then
    sum=(sha256sum)
else
    sum=(shasum -a 256)
fi
(cd "$dir" && "${sum[@]}" clank-"$version"-*.tar.gz) >"$dir/SHA256SUMS.part"
mv "$dir/SHA256SUMS.part" "$dir/SHA256SUMS"
cat "$dir/SHA256SUMS"
