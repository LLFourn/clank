#!/usr/bin/env bash
# Print a release's notes: its CHANGELOG.md section, without the heading.
# Usage: release-notes.sh <version> [changelog]
set -euo pipefail

version="${1:?usage: release-notes.sh <version> [changelog]}"
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
changelog="${2:-$root/CHANGELOG.md}"

notes="$(awk -v head="## [$version]" '
    $0 == head { inside = 1; next }
    inside && /^## \[/ { exit }
    inside { print }
' "$changelog" | sed -e '/./,$!d')"
if [[ -z "${notes//[[:space:]]/}" ]]; then
    echo "$changelog has no notes under '## [$version]'" >&2
    exit 1
fi
printf '%s\n' "$notes"
