#!/usr/bin/env bash
# open-worktree.sh — create or open a clank worktree as a new zellij tab,
# with one pane per agent configured in the main repo's clank state.
#
# Usage:
#     ./open-worktree.sh <name>
#
# Reads the agent list from `clank open --json <main-repo>`:
#   - master goes in the top pane
#   - all other agents are stacked below as reviewer panes
# So if you swap codex for gemini (or add a third reviewer), this script
# follows automatically — no hardcoded tool names.

set -euo pipefail

if [[ $# -ne 1 ]]; then
    echo "usage: $0 <name>" >&2
    exit 1
fi

if ! command -v jq >/dev/null; then
    echo "ERROR: jq not found. Install with: brew install jq" >&2
    exit 1
fi

NAME="$1"
REPO_ROOT="$(git rev-parse --show-toplevel)"
WT="$REPO_ROOT/.clank/worktrees/$NAME"

# 1. Create the worktree.
if [[ ! -d "$WT" ]]; then
    echo "Creating git worktree at $WT"
    git -C "$REPO_ROOT" worktree add -b "$NAME" "$WT"
else
    echo "Worktree already exists: $WT"
fi

# 2. Bootstrap clank state in the worktree.
if [[ ! -d "$WT/.clank" ]]; then
    echo "Running clank init in worktree"
    (cd "$WT" && clank init)
else
    echo ".clank/ already present in worktree"
fi

# 3. Discover agents from the main repo's clank state.
OPEN_JSON="$(clank open dry --json "$REPO_ROOT")"
MASTER_LABEL=$(echo "$OPEN_JSON" | jq -r '.clank.master_agents[0] // empty')

if [[ -z "$MASTER_LABEL" ]]; then
    echo "ERROR: no master agent configured for $REPO_ROOT" >&2
    exit 1
fi

MASTER_TOOL=$(echo "$OPEN_JSON" | jq -r --arg m "$MASTER_LABEL" \
    '.clank.agents[] | select(.label == $m) | .tool')

REVIEWERS=$(echo "$OPEN_JSON" | jq -r --arg m "$MASTER_LABEL" \
    '.clank.agents[] | select(.label != $m) | "\(.label)|\(.tool)"')

# 4. Generate a per-tab layout string: master on top, reviewers stacked below,
#    wrapped by tab-bar + status-bar plugin panes so the tab matches the
#    rest of the session's chrome.
LAYOUT_KDL=$(
    {
        echo 'layout {'
        echo '    pane size=1 borderless=true {'
        echo '        plugin location="zellij:tab-bar"'
        echo '    }'
        echo '    pane split_direction="horizontal" {'
        cat <<KDL
        pane name="$MASTER_LABEL (master)" {
            command "$MASTER_TOOL"
            args "Please run \`clank as $MASTER_LABEL\` to bind this session as the master agent for this worktree. Reply with the result, then wait for further instructions."
        }
KDL
        while IFS='|' read -r label tool; do
            [[ -z "$label" ]] && continue
            cat <<KDL
        pane name="$label (reviewer)" {
            command "$tool"
            args "Please run \`clank as $label\` to bind this session as a reviewer agent for this worktree. Reply with the result, then wait for further instructions."
        }
KDL
        done <<<"$REVIEWERS"
        echo '    }'
        echo '    pane size=2 borderless=true {'
        echo '        plugin location="zellij:status-bar"'
        echo '    }'
        echo '}'
    }
)

# 5. Sanity-check we're inside zellij, then spawn the tab.
if [[ -z "${ZELLIJ_SESSION_NAME:-}" ]]; then
    echo "ERROR: not inside a zellij session (ZELLIJ_SESSION_NAME unset)." >&2
    echo "Run this script from a shell pane inside zellij." >&2
    exit 1
fi

echo "Opening tab '$NAME' in zellij session '$ZELLIJ_SESSION_NAME'"
echo "Master: $MASTER_LABEL ($MASTER_TOOL)"
echo "Reviewers:"
echo "$REVIEWERS" | sed 's/^/  /'

zellij action new-tab --cwd "$WT" --name "$NAME" --layout-string "$LAYOUT_KDL"

cat <<EOF

Tab '$NAME' opened. Alt-o to switch to it.

Setup done by this script:
  - git worktree created (or already existed)
  - clank init run in the worktree (.clank/ scaffolded)
  - $MASTER_LABEL launched with a seed prompt to run 'clank as $MASTER_LABEL'
  - each reviewer launched with a seed prompt to run 'clank as <label>'

So no manual binding required — the agents should self-bind on their first turn.
EOF
