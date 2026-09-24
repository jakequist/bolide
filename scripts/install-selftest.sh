#!/usr/bin/env bash
# Exercise install.sh without a network: a fake `uname` and `sysctl` on PATH, a `curl`
# that refuses every https URL, and the releases page served from loopback by a small
# python3 fixture server that redirects `releases/latest` the way github.com does.
#
#   bash scripts/install-selftest.sh
#
# CI runs it on Linux and on macOS, where /bin/sh is a different shell again.
set -uo pipefail

HERE=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
INSTALL="$HERE/../install.sh"
W=$(mktemp -d)
SERVER_PID=
cleanup() {
  if [ -n "$SERVER_PID" ]; then kill "$SERVER_PID" 2>/dev/null; fi
  rm -rf "$W"
}
trap cleanup EXIT
REAL_CURL=$(command -v curl)
command -v python3 >/dev/null 2>&1 || {
  echo "install-selftest: python3 is needed for the fixture server" >&2
  exit 1
}

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
# packs them, each holding a `bolide` that prints which one it is. `site/<repo>/releases`
# is one repository's releases page; its LATEST file names the tag `releases/latest`
# redirects to, and a repository without one has no release yet.
REL="$W/site/gh/releases"
for t in aarch64-apple-darwin x86_64-apple-darwin x86_64-unknown-linux-musl aarch64-unknown-linux-musl; do
  for v in 0.1.0 0.2.0; do
    mkdir -p "$REL/download/v$v" "$W/stage/$t-$v"
    printf '#!/bin/sh\necho "bolide %s %s"\n' "$v" "$t" >"$W/stage/$t-$v/bolide"
    chmod +x "$W/stage/$t-$v/bolide"
    tar -czf "$REL/download/v$v/bolide-$v-$t.tar.gz" -C "$W/stage/$t-$v" bolide
  done
done
echo v0.2.0 >"$REL/LATEST"
mkdir -p "$W/site/unreleased/releases"

# The fixture server, on a kernel-assigned loopback port. `releases/latest` answers the
# way github.com does: a 302 (lower-case header, as HTTP/2 sends it) to
# `releases/tag/<tag>`, or to `releases` when nothing is released. Every request path is
# logged. It prints its port once bound, and the `read` below blocks on exactly that.
cat >"$W/server.py" <<'PY'
import http.server, os, sys

ROOT, LOG = sys.argv[1], sys.argv[2]


class Handler(http.server.SimpleHTTPRequestHandler):
    def __init__(self, *args, **kwargs):
        super().__init__(*args, directory=ROOT, **kwargs)

    def log_message(self, *args):
        pass

    def answer(self, status, location=None):
        self.send_response(status)
        if location:
            self.send_header("location", location)
        self.send_header("content-length", "0")
        self.end_headers()

    def route(self):
        with open(LOG, "a") as log:
            log.write(self.path + "\n")
        path = self.path.split("?", 1)[0]
        base = "http://%s:%d" % self.server.server_address[:2]
        if path.endswith("/releases/latest"):
            releases = path[: -len("/latest")]
            latest = os.path.join(ROOT, releases.lstrip("/"), "LATEST")
            if os.path.exists(latest):
                with open(latest) as f:
                    tag = f.read().strip()
                self.answer(302, "%s%s/tag/%s" % (base, releases, tag))
            else:
                self.answer(302, base + releases)
            return True
        if "/releases/tag/" in path or path.endswith("/releases"):
            self.answer(200)
            return True
        return False

    def do_GET(self):
        if not self.route():
            super().do_GET()

    def do_HEAD(self):
        if not self.route():
            super().do_HEAD()


server = http.server.HTTPServer(("127.0.0.1", 0), Handler)
print(server.server_address[1], flush=True)
sys.stdout.close()
server.serve_forever()
PY
mkfifo "$W/port"
python3 "$W/server.py" "$W/site" "$W/requests.log" >"$W/port" 2>"$W/server.err" &
SERVER_PID=$!
read -r PORT <"$W/port" || {
  echo "install-selftest: the fixture server did not start: $(cat "$W/server.err")" >&2
  exit 1
}
SITE="http://127.0.0.1:$PORT"

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
  # Everything is on the loopback fixture, so any https URL is a test bug, not a
  # download — and the rate-limited GitHub API is recorded so the run fails on it.
  cat >"$b/curl" <<EOF
#!/bin/sh
for a in "\$@"; do
  case "\$a" in
    *api.github.com*) echo "\$a" >>"$W/api-calls"; echo "offline selftest: install.sh asked the GitHub API" >&2; exit 22 ;;
    https://*) echo "offline selftest: no network for \$a" >&2; exit 7 ;;
  esac
done
exec "$REAL_CURL" "\$@"
EOF
  chmod +x "$b/uname" "$b/sysctl" "$b/curl"
  printf '%s' "$b"
}

# install OS ARCH ROSETTA VERSION [REPO] — runs install.sh against the fixture's REPO
# (default gh); sets OUT, RC and DIR.
install() {
  local b
  b=$(fakebin "$1" "$2" "$3")
  DIR="$W/out/$1-$2-$3-${4:-latest}-${5:-gh}"
  OUT=$(PATH="$b:$PATH" BOLIDE_VERSION="$4" BOLIDE_RELEASES_URL="$SITE/${5:-gh}/releases" \
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
  ok "no BOLIDE_VERSION installs the tag releases/latest redirects to"
else
  bad "latest: rc=$RC, $OUT"
fi
if grep -qx "/gh/releases/latest" "$W/requests.log" 2>/dev/null; then
  ok "and it asked the releases page for it"
else
  bad "latest was not resolved through /releases/latest: $(cat "$W/requests.log" 2>/dev/null)"
fi

echo 0.1.0/ >"$REL/LATEST"
install Linux x86_64 0 "" gh
if [ "$RC" -eq 0 ] && [ "$(installed)" = "bolide 0.1.0 x86_64-unknown-linux-musl" ]; then
  ok "a tag without a v, and a trailing slash on the redirect, still resolve"
else
  bad "unprefixed latest: rc=$RC, $OUT"
fi
echo v0.2.0 >"$REL/LATEST"

install Linux x86_64 0 "" unreleased
case "$RC:$OUT" in
  0:*) bad "a repository with no release installed something" ;;
  *"no release"*"BOLIDE_VERSION"*) ok "a repository with no release fails, saying so" ;;
  *) bad "no release failed unhelpfully: $OUT" ;;
esac

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

if [ -e "$W/api-calls" ]; then
  bad "install.sh contacted the GitHub API: $(cat "$W/api-calls")"
else
  ok "nothing contacted the GitHub API"
fi

echo
echo "$PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
