#!/usr/bin/env bash
# Regenerate crates/cli/notices/rust-dependencies.txt from Cargo.lock.
# Usage: generate.sh [OUTPUT]
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
root="$(cd "$here/../.." && pwd -P)"
out="${1:-$root/crates/cli/notices/rust-dependencies.txt}"
pinned="$(cat "$here/cargo-about.version")"

have="$(cargo about --version 2>/dev/null | awk '{print $2}')" || true
if [[ "$have" != "$pinned" ]]; then
    echo "need cargo-about $pinned (found '${have:-none}'):" >&2
    echo "  cargo install cargo-about --locked --features cli --version $pinned" >&2
    exit 1
fi

targets=$(sed -n '/^targets = \[/,/^\]/p' "$here/about.toml" | grep -o '"[^"]*"' | tr -d '"')

# Every platform's crates, not only this host's: the notice covers the
# macOS and Linux targets wherever it is generated.
cargo fetch --locked --manifest-path "$root/Cargo.toml"

json="$(mktemp)"
trap 'rm -f "$json"' EXIT
# --frozen: offline and locked, so the output depends on Cargo.lock and
# the crates' own files, never on what a network fetch returns today.
cargo about generate --frozen \
    --manifest-path "$root/Cargo.toml" \
    -c "$here/about.toml" \
    --format json -o "$json"
members="$(cargo metadata --frozen --no-deps --format-version 1 --manifest-path "$root/Cargo.toml" |
    python3 -B -I -c 'import json, sys; print("\n".join(json.load(sys.stdin)["workspace_members"]))')"
mkdir -p "$(dirname "$out")"
# shellcheck disable=SC2086
CLANK_WORKSPACE_MEMBERS="$members" python3 -B -I "$here/render.py" "$pinned" $targets < "$json" > "$out"
