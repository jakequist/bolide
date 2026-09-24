#!/usr/bin/env bash
# Exercise install.sh without a network: a fake `uname`, `sysctl` and (for the GitHub API
# call only) `curl` on PATH, and the releases page as a file:// fixture.
#
#   bash scripts/install-selftest.sh
#
# CI runs it on Linux and on macOS, where /bin/sh is a different shell again.
set -uo pipefail

HERE=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
INSTALL="$HERE/../install.sh"
W=$(mktemp -d)
trap 'rm -rf "$W"' EXIT
REAL_CURL=$(command -v curl)

PASS=0
FAIL=0
ok() {
  PASS=$((PASS + 1))
  echo "  ok   $1"
}
bad() {
  FAIL=$((FAIL + 1))
  echo "  FAIL $1"
}

# The releases fixture: one tarball per target and version, packed the way release.yml
# packs them, each holding a `bolide` that prints which one it is.
for t in aarch64-apple-darwin x86_64-apple-darwin x86_64-unknown-linux-musl aarch64-unknown-linux-musl; do
  for v in 0.1.0 0.2.0; do
    mkdir -p "$W/releases/download/v$v" "$W/stage/$t-$v"
    printf '#!/bin/sh\necho "bolide %s %s"\n' "$v" "$t" >"$W/stage/$t-$v/bolide"
    chmod +x "$W/stage/$t-$v/bolide"
    tar -czf "$W/releases/download/v$v/bolide-$v-$t.tar.gz" -C "$W/stage/$t-$v" bolide
  done
done

# fakebin OS ARCH ROSETTA — a PATH directory that makes this machine look like OS/ARCH.
fakebin() {
  local b="$W/fake/$1-$2-$3"
  mkdir -p "$b"
  cat >"$b/uname" <<EOF
#!/bin/sh
case "\$1" in -m) echo $2 ;; *) echo $1 ;; esac
EOF
  if [ "$3" = 1 ]; then
    printf '#!/bin/sh\necho 1\n' >"$b/sysctl"
  else
    printf '#!/bin/sh\nexit 1\n' >"$b/sysctl"
  fi
  # The API answers with v0.2.0; any other https URL is a test bug, not a download.
  cat >"$b/curl" <<EOF
#!/bin/sh
for a in "\$@"; do
  case "\$a" in
    https://api.github.com/*) printf '{\n  "url": "x",\n  "tag_name": "v0.2.0",\n  "name": "v0.2.0"\n}\n'; exit 0 ;;
    https://*) echo "offline selftest: no network for \$a" >&2; exit 7 ;;
  esac
done
exec "$REAL_CURL" "\$@"
EOF
  chmod +x "$b/uname" "$b/sysctl" "$b/curl"
  printf '%s' "$b"
}

# install OS ARCH ROSETTA VERSION — runs install.sh; sets OUT, RC and DIR.
install() {
  local b
  b=$(fakebin "$1" "$2" "$3")
  DIR="$W/out/$1-$2-$3-${4:-latest}"
  OUT=$(PATH="$b:$PATH" BOLIDE_VERSION="$4" BOLIDE_RELEASES_URL="file://$W/releases" \
    BOLIDE_INSTALL_DIR="$DIR" sh "$INSTALL" 2>&1)
  RC=$?
}

installed() { [ -x "$DIR/bolide" ] && "$DIR/bolide"; }

expect_target() {
  install "$1" "$2" "$3" 0.1.0
  if [ "$RC" -eq 0 ] && [ "$(installed)" = "bolide 0.1.0 $4" ]; then
    ok "$1/$2 (rosetta=$3) installs $4"
  else
    bad "$1/$2 (rosetta=$3): rc=$RC, $OUT"
  fi
}

expect_target Darwin arm64 0 aarch64-apple-darwin
expect_target Darwin x86_64 0 x86_64-apple-darwin
expect_target Darwin x86_64 1 aarch64-apple-darwin
expect_target Linux x86_64 0 x86_64-unknown-linux-musl
expect_target Linux amd64 0 x86_64-unknown-linux-musl
expect_target Linux aarch64 0 aarch64-unknown-linux-musl
expect_target Linux arm64 0 aarch64-unknown-linux-musl

install FreeBSD amd64 0 0.1.0
case "$RC:$OUT" in
  0:*) bad "an unsupported platform installed something" ;;
  *"no prebuilt bolide for FreeBSD/amd64"*"cargo install"*) ok "an unsupported platform fails, naming it and the source build" ;;
  *) bad "an unsupported platform failed unhelpfully: $OUT" ;;
esac

install Linux armv7l 0 0.1.0
if [ "$RC" -ne 0 ]; then ok "32-bit arm fails rather than installing a wrong binary"; else bad "armv7l installed"; fi

install Linux x86_64 0 ""
if [ "$RC" -eq 0 ] && [ "$(installed)" = "bolide 0.2.0 x86_64-unknown-linux-musl" ]; then
  ok "no BOLIDE_VERSION installs the latest tag the API names"
else
  bad "latest: rc=$RC, $OUT"
fi

install Linux x86_64 0 v0.1.0
if [ "$RC" -eq 0 ] && [ "$(installed)" = "bolide 0.1.0 x86_64-unknown-linux-musl" ]; then
  ok "a v-prefixed BOLIDE_VERSION works"
else
  bad "v-prefixed pin: $OUT"
fi

install Linux x86_64 0 9.9.9
case "$RC:$OUT" in
  0:*) bad "a missing version reported success" ;;
  *"could not download"*) ok "a missing version fails, naming the download" ;;
  *) bad "a missing version failed unhelpfully: $OUT" ;;
esac
if [ ! -e "$DIR/bolide" ]; then ok "and installs nothing"; else bad "a failed install left a binary"; fi

install Linux x86_64 0 0.1.0
install Linux x86_64 0 0.2.0
if [ "$(installed)" = "bolide 0.2.0 x86_64-unknown-linux-musl" ] && [ ! -e "$DIR/bolide.new" ]; then
  ok "a reinstall replaces the binary in place and leaves no staged copy"
else
  bad "reinstall: $OUT"
fi

echo
echo "$PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
