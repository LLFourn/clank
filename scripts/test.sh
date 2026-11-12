#!/usr/bin/env bash
# Own a short, private runtime namespace for this test run and its children.
# Usage: scripts/test.sh [cargo test arguments...]
# Do not point tests at a live Clank runtime directory.
set -euo pipefail

test_runtime=$(mktemp -d /tmp/clank-test.XXXXXX)
trap 'rm -rf "$test_runtime"' EXIT
export CLANK_TEST_RUNTIME="$test_runtime"
cargo test "$@"
