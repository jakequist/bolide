#!/bin/sh
# bolide installer — https://github.com/jakequist/bolide
#
#   curl -fsSL https://raw.githubusercontent.com/jakequist/bolide/main/install.sh | sh
#
# Downloads the prebuilt bolide binary for this platform from the GitHub releases and
# puts it on PATH. Settings, all optional:
#   BOLIDE_VERSION       install this version (e.g. 0.1.0) instead of the latest release
#   BOLIDE_INSTALL_DIR   install here instead of /usr/local/bin, or ~/.local/bin when
#                        /usr/local/bin is not writable
#   BOLIDE_RELEASES_URL  download from this mirror of the releases page instead of GitHub
#
# Once installed, `bolide update` keeps it current.
set -eu

REPO="jakequist/bolide"
RELEASES="${BOLIDE_RELEASES_URL:-https://github.com/$REPO/releases}"

fail() {
  printf 'install.sh: %s\n' "$1" >&2
  exit 1
}

command -v curl >/dev/null 2>&1 || fail "curl is required but was not found on PATH."
command -v tar >/dev/null 2>&1 || fail "tar is required but was not found on PATH."

os=$(uname -s)
arch=$(uname -m)
# A shell running under Rosetta on Apple Silicon reports x86_64; the native build is the
# one to install.
if [ "$os" = Darwin ] && [ "$arch" = x86_64 ] &&
  [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || true)" = 1 ]; then
  arch=arm64
fi

case "$os/$arch" in
  Darwin/arm64 | Darwin/aarch64) target="aarch64-apple-darwin" ;;
  Darwin/x86_64) target="x86_64-apple-darwin" ;;
  Linux/x86_64 | Linux/amd64) target="x86_64-unknown-linux-musl" ;;
  Linux/aarch64 | Linux/arm64) target="aarch64-unknown-linux-musl" ;;
  *) fail "there is no prebuilt bolide for $os/$arch.
Prebuilt binaries exist for macOS (arm64, x86_64) and Linux (x86_64, aarch64).
Build it from source instead (needs a Rust toolchain):
  cargo install --git https://github.com/$REPO bolide-cli" ;;
esac

version="${BOLIDE_VERSION-}"
if [ -z "$version" ]; then
  # releases/latest redirects to releases/tag/<tag>; curl reports where it landed, so no
  # header is parsed here. Not the GitHub API: that allows 60 unauthenticated calls an
  # hour per IP, and behind a shared IP it answers 403. crates/bolide-cli/src/update.rs
  # (`bolide update`) resolves the latest release the same way.
  latest=$(curl -fsSL --max-time 15 -o /dev/null -w '%{url_effective}' "$RELEASES/latest") ||
    fail "could not ask GitHub for the latest bolide release.
Check your network and try again, or pick a version from $RELEASES and set BOLIDE_VERSION."
  tag=${latest%%[?#]*}
  tag=${tag%/}
  case "$tag" in
    */releases/tag/?*) tag=${tag##*/releases/tag/} ;;
    *) tag= ;;
  esac
  case "$tag" in
    "" | v | */*) fail "found no release tag at $RELEASES/latest (it led to $latest), so there is no release yet.
Pick a version from $RELEASES and set BOLIDE_VERSION." ;;
  esac
  version=$tag
fi
version="${version#v}"

tmp=$(mktemp -d 2>/dev/null || mktemp -d -t bolide)
trap 'rm -rf "$tmp"' EXIT INT TERM

# The asset name is also stated in .github/workflows/release.yml (which packs it) and
# crates/bolide-cli/src/update.rs asset_name (which `bolide update` downloads).
url="$RELEASES/download/v$version/bolide-$version-$target.tar.gz"
printf 'Downloading bolide %s (%s)...\n' "$version" "$target"
curl -fsSL --max-time 300 -o "$tmp/bolide.tar.gz" "$url" ||
  fail "could not download $url
Check the version and your network, or see $RELEASES"
[ -s "$tmp/bolide.tar.gz" ] || fail "the download from $url was empty."
tar -xzf "$tmp/bolide.tar.gz" -C "$tmp" || fail "could not unpack $url"
[ -f "$tmp/bolide" ] || fail "that release archive holds no bolide binary. See $RELEASES"

dir="${BOLIDE_INSTALL_DIR-}"
if [ -z "$dir" ]; then
  for candidate in /usr/local/bin "$HOME/.local/bin"; do
    if [ -d "$candidate" ] && [ -w "$candidate" ]; then
      dir="$candidate"
      break
    fi
  done
  [ -n "$dir" ] || dir="$HOME/.local/bin"
fi
mkdir -p "$dir" || fail "could not create $dir. Set BOLIDE_INSTALL_DIR to a writable directory and re-run."

# Staged beside the target and renamed, so an interrupted install never leaves a
# half-written bolide on PATH.
cp "$tmp/bolide" "$dir/bolide.new" ||
  fail "could not write to $dir. Set BOLIDE_INSTALL_DIR to a writable directory, or re-run with sudo."
chmod 755 "$dir/bolide.new"
mv -f "$dir/bolide.new" "$dir/bolide" || fail "could not install into $dir."

printf 'Installed bolide %s to %s\n' "$version" "$dir/bolide"
case ":${PATH-}:" in
  *":$dir:"*) printf 'Get started: bolide connect vnc://HOST\n' ;;
  *) printf 'Add %s to your PATH, then run: bolide connect vnc://HOST\n  export PATH="%s:%s"\n' "$dir" "$dir" "\$PATH" ;;
esac
