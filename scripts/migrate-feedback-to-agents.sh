#!/usr/bin/env bash
# Migrate `.clank/feedback/<target>/<sha>/<author>.md` to
# `.clank/agents/<author>/feedback/<target>/<sha>.md`.
#
# `<target>` is either a plan stem or the reserved `_` (ad-hoc).
# Both kinds are migrated identically; the script doesn't care
# which.
#
# Uses plain `mv` (filesystem rename), NOT `git mv`. Feedback
# files are gitignored (`.clank/feedback/` is matched by the
# root `.clank/*` rule), so `git mv` would refuse to move them.
# The new `.clank/agents/` tree is also gitignored.
#
# Idempotent: re-running on an already-migrated repo finds
# nothing to do.
#
# Usage:
#   scripts/migrate-feedback-to-agents.sh [<repo-root>]
#
# If <repo-root> is omitted, uses `git rev-parse --show-toplevel`
# from the cwd.

set -euo pipefail

repo_root="${1:-}"
if [ -z "$repo_root" ]; then
    repo_root="$(git rev-parse --show-toplevel)"
fi

old_root="$repo_root/.clank/feedback"
if [ ! -d "$old_root" ]; then
    echo "no .clank/feedback/ to migrate (already clean)" >&2
    exit 0
fi

moved=0
# Shape: .clank/feedback/<target>/<sha>/<author>.md
find "$old_root" -mindepth 3 -maxdepth 3 -type f -name '*.md' | while read -r old; do
    rel="${old#"$old_root/"}"
    target="${rel%%/*}"
    rest="${rel#*/}"
    sha="${rest%%/*}"
    author_md="${rest#*/}"
    author="${author_md%.md}"

    new_dir="$repo_root/.clank/agents/$author/feedback/$target"
    new_path="$new_dir/$sha.md"

    mkdir -p "$new_dir"
    mv "$old" "$new_path"
    echo "  $rel -> agents/$author/feedback/$target/$sha.md" >&2
    moved=$((moved + 1))
done

# Tear down now-empty .clank/feedback/. `find -depth -empty`
# only removes dirs that have no remaining files.
find "$old_root" -depth -type d -empty -delete 2>/dev/null || true

echo "migration done" >&2
