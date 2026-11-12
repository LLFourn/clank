#!/usr/bin/env bash
# One orchestration smoke check outside the ordinary Rust suite. Start with an
# empty Cargo home, populate only production dependencies, then invoke the real
# release entry point. Never copy or remove entries in the developer's cache.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
smoke_root=$(mktemp -d)
trap 'rm -rf "$smoke_root"' EXIT
export CARGO_HOME="$smoke_root/cargo"
export CARGO_TARGET_DIR="$smoke_root/target"
mkdir -p "$CARGO_HOME"

cargo build --locked --manifest-path "$root/Cargo.toml" \
    -p clank --bin clank --bin clank-web-cloudflare

# This deliberately invalid tag must reach the Rust validator. A dependency
# bootstrap failure also exits unsuccessfully, so the diagnostic is essential.
status=0
"$root/scripts/release-check.sh" entrypoint-smoke \
    "$CARGO_TARGET_DIR/debug/clank" > "$smoke_root/result" 2>&1 || status=$?
cat "$smoke_root/result"
if [[ "$status" != 1 ]] || ! grep -qF \
    "tag 'entrypoint-smoke' is not v<MAJOR>.<MINOR>.<PATCH>" "$smoke_root/result"; then
    echo "release entry point failed before validating its input" >&2
    exit 1
fi
echo "release entry point bootstrapped from a production-only cache"
