#!/usr/bin/env bash
# Fail when crates/cli/notices/rust-dependencies.txt is not what
# generate.sh would write for the current Cargo.lock.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
root="$(cd "$here/../.." && pwd -P)"
committed="$root/crates/cli/notices/rust-dependencies.txt"

fresh="$(mktemp)"
trap 'rm -f "$fresh"' EXIT
"$here/generate.sh" "$fresh"

if ! diff -u "$committed" "$fresh"; then
    echo >&2
    echo "crates/cli/notices/rust-dependencies.txt is stale: run scripts/notices/generate.sh" >&2
    exit 1
fi
