#!/bin/sh
# A clean first-run walk environment for one harness (docs/first-run-walk.md).
# Usage: env.sh [--print] claude|codex|grok|opencode
#
# The shell it opens starts from `env -i`: only PATH, TERM, LANG and USER
# survive, HOME is a new directory, and every provider root resolves under
# that HOME. Replacing HOME alone is not enough: clank's setup gives each
# provider-root variable precedence over HOME (`resolve_provider_root` in
# crates/cli/src/cli/setup.rs), so an inherited one would reach the real
# profile. Nothing is copied from the real HOME; each harness logs in afresh.
#
# --print prints what it would use and creates and launches nothing.
set -eu

usage() {
    echo "usage: $0 [--print] claude|codex|grok|opencode" >&2
    exit 2
}

print_only=0
if [ "${1:-}" = "--print" ]; then
    print_only=1
    shift
fi
[ $# -eq 1 ] || usage
harness=$1
case $harness in
    claude | codex | grok | opencode) ;;
    *) usage ;;
esac

stamp=$(od -An -N3 -tx1 /dev/urandom | tr -d ' \n')
tmp=${TMPDIR:-/tmp}
tmp=${tmp%/}
root=$tmp/clank-first-run-$harness-$stamp
home=$root/home
# `clank open` names its zellij session `clank-<repo basename>`, at most 24
# characters (SESSION_NAME_MAX in crates/cli/src/cli/open_zellij.rs), so
# the repo's basename is what makes the session unique.
repo=$root/fr-$harness-$stamp
session=clank-fr-$harness-$stamp

set -- HOME="$home" PATH="$home/.local/bin:${PATH:-/usr/bin:/bin}"
if [ -n "${TERM+x}" ]; then set -- "$@" "TERM=$TERM"; fi
if [ -n "${LANG+x}" ]; then set -- "$@" "LANG=$LANG"; fi
if [ -n "${USER+x}" ]; then set -- "$@" "USER=$USER"; fi

echo "harness $harness"
echo "root $root"
echo "home $home"
echo "repo $repo"
echo "session $session"
for assignment in "$@"; do
    echo "set $assignment"
done
echo "unset CLAUDE_CONFIG_DIR resolves to $home/.claude"
echo "unset CODEX_HOME resolves to $home/.codex"
echo "unset GROK_HOME resolves to $home/.grok"
echo "unset OPENCODE_CONFIG_DIR resolves to $home/.config/opencode"
echo "unset XDG_CONFIG_HOME resolves to $home/.config"
for name in CLAUDE_CODE_SESSION_ID CODEX_THREAD_ID OPENCODE_SESSION_ID \
    GROK_AGENT GROK_SESSION_ID CLANK_AGENT CLANK_BOOTSTRAP_SESSION_ID; do
    echo "unset $name"
done

[ "$print_only" -eq 0 ] || exit 0

if [ -e "$root" ]; then
    echo "$root already exists" >&2
    exit 1
fi
umask 077
mkdir "$root" "$home"
in_clean() {
    env -i HOME="$home" PATH="${PATH:-/usr/bin:/bin}" "$@"
}
in_clean git init --quiet --initial-branch=main "$repo"
in_clean git -C "$repo" config user.name "first-run walk"
in_clean git -C "$repo" config user.email "first-run@example.invalid"
in_clean git -C "$repo" config commit.gpgsign false
in_clean git -C "$repo" commit --quiet --allow-empty -m "first-run walk"

echo "entering the clean shell in $repo; exit to leave it"
cd "$repo"
exec env -i "$@" /bin/sh -i
