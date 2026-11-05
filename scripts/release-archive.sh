#!/usr/bin/env bash
# Pack one target's release archive.
# Usage: release-archive.sh <version> <target> <bin-dir> <out-dir>
# Writes <out-dir>/clank-<version>-<target>.tar.gz: one clank-<version>-<target>/
# directory holding clank, clank-web-cloudflare, LICENSE, THIRD-PARTY-NOTICES.md
# and the Rust dependency notice at the path THIRD-PARTY-NOTICES.md links.
set -euo pipefail

usage="usage: release-archive.sh <version> <target> <bin-dir> <out-dir>"
version="${1:?$usage}"
target="${2:?$usage}"
bin_dir="${3:?$usage}"
out_dir="${4:?$usage}"
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"

name="clank-$version-$target"
stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT
mkdir -p "$stage/$name/crates/cli/notices" "$out_dir"
for bin in clank clank-web-cloudflare; do
    install -m 755 "$bin_dir/$bin" "$stage/$name/$bin"
done
install -m 644 "$root/LICENSE" "$root/THIRD-PARTY-NOTICES.md" "$stage/$name/"
install -m 644 "$root/crates/cli/notices/rust-dependencies.txt" "$stage/$name/crates/cli/notices/"
# macOS tar would otherwise add AppleDouble files and xattr headers that
# GNU tar warns about.
COPYFILE_DISABLE=1 tar --no-xattrs -C "$stage" -czf "$out_dir/$name.tar.gz" "$name"
echo "$out_dir/$name.tar.gz"
