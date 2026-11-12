#!/bin/sh
# Install clank's prebuilt binaries from its GitHub release.
#
#   curl -fsSL https://llfourn.github.io/clank/install.sh | sh
#
# CLANK_VERSION=0.2.0 installs that release instead of the latest.
# CLANK_INSTALL_DIR=<dir> installs there instead of ~/.local/bin.
#
# Test-only, for the installer's own tests: CLANK_RELEASE_BASE and
# CLANK_API_BASE replace the GitHub URLs, and CLANK_TEST_UNAME_S and
# CLANK_TEST_UNAME_M replace `uname -s` and `uname -m`.
set -eu

release_base="${CLANK_RELEASE_BASE:-https://github.com/LLFourn/clank/releases}"
api_base="${CLANK_API_BASE:-https://api.github.com/repos/LLFourn/clank}"

fail() {
    echo "clank install: $*" >&2
    exit 1
}

command -v curl >/dev/null 2>&1 || fail "curl is required"
command -v tar >/dev/null 2>&1 || fail "tar is required"

# A redirect may not downgrade a download to plain http.
fetch() {
    curl --proto-redir '=https' -fsSL --retry 3 "$@"
}

os="${CLANK_TEST_UNAME_S:-$(uname -s)}"
arch="${CLANK_TEST_UNAME_M:-$(uname -m)}"
case "$os" in
Darwin) os_part=apple-darwin ;;
Linux) os_part=unknown-linux-musl ;;
*) os_part= ;;
esac
case "$arch" in
arm64 | aarch64) arch_part=aarch64 ;;
x86_64 | amd64) arch_part=x86_64 ;;
*) arch_part= ;;
esac
if [ -z "$os_part" ] || [ -z "$arch_part" ]; then
    fail "no prebuilt binary for $os $arch; build from source instead: https://github.com/LLFourn/clank#install"
fi
target="$arch_part-$os_part"

if [ -n "${CLANK_VERSION:-}" ]; then
    version="${CLANK_VERSION#v}"
else
    # One concrete version, so the archive and SHA256SUMS come from the
    # same release even if a newer one is published mid-install.
    latest="$(fetch "$api_base/releases/latest")" || fail "could not look up the latest release"
    version="$(printf '%s\n' "$latest" | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"v\([^"]*\)".*/\1/p' | head -n 1)"
fi
case "$version" in
'' | *[!0-9.]* | .* | *. | *..*) fail "'$version' is not a release version like 0.1.0" ;;
esac

if command -v sha256sum >/dev/null 2>&1; then
    sha256() { sha256sum "$1" | cut -d ' ' -f 1; }
elif command -v shasum >/dev/null 2>&1; then
    sha256() { shasum -a 256 "$1" | cut -d ' ' -f 1; }
else
    fail "sha256sum or shasum is required to verify the download"
fi

name="clank-$version-$target"
archive="$name.tar.gz"
dir="${CLANK_INSTALL_DIR:-$HOME/.local/bin}"

tmp="$(mktemp -d)"
staging=
cleanup() {
    rm -rf "$tmp"
    if [ -n "$staging" ]; then
        rm -f "$dir/.clank.install.$$" "$dir/.clank-web-cloudflare.install.$$"
    fi
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM

url="$release_base/download/v$version"
echo "clank install: downloading $archive"
fetch -o "$tmp/$archive" "$url/$archive" || fail "could not download $url/$archive"
fetch -o "$tmp/SHA256SUMS" "$url/SHA256SUMS" || fail "could not download $url/SHA256SUMS"

want="$(awk -v f="$archive" '$2 == f || $2 == "*" f { print $1 }' "$tmp/SHA256SUMS")"
[ -n "$want" ] || fail "SHA256SUMS has no entry for $archive"
got="$(sha256 "$tmp/$archive")"
[ "$got" = "$want" ] || fail "$archive does not match its SHA256SUMS entry (got $got, want $want); nothing was installed"

mkdir "$tmp/x"
tar -xzf "$tmp/$archive" -C "$tmp/x"
for bin in clank clank-web-cloudflare; do
    [ -f "$tmp/x/$name/$bin" ] || fail "$archive has no $bin"
done

mkdir -p "$dir"
staging=1
# Both are staged beside their destination before either is renamed
# into place, so a failed copy leaves the old install whole.
for bin in clank clank-web-cloudflare; do
    cp "$tmp/x/$name/$bin" "$dir/.$bin.install.$$"
    chmod 755 "$dir/.$bin.install.$$"
done
for bin in clank clank-web-cloudflare; do
    mv -f "$dir/.$bin.install.$$" "$dir/$bin"
done
staging=

echo "clank install: installed clank $version ($target) to $dir"
case ":$PATH:" in
*":$dir:"*) ;;
*) echo "clank install: $dir is not on your PATH; add it, for example: export PATH=\"$dir:\$PATH\"" ;;
esac
