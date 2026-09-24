# bolide

**Point an agent at any VNC desktop.**

`bolide` connects to an RFB (VNC) server — a Mac, a Linux box, a container, a VM — and
runs a small loopback HTTP server that speaks the **computer-use protocol**. An agent
drives the remote desktop by POSTing the same `computer` tool input it would send
anywhere else. The same binary is also the client, so you can drive the desktop by hand
while the agent works.

```console
$ bolide connect vnc://mac01.local --username jake --password-file ~/.vnc-pass
connected to mac01.local:5900 (1728x1117, "mac01")
computer-use on http://127.0.0.1:53211

$ bolide screenshot --out screen.png
wrote screen.png (1728x1117)

$ bolide click 420 180
$ bolide type "hello"
$ bolide key cmd+space
$ bolide clipboard push "pasted by bolide"
```

And from the agent's side, unchanged from the `computer` tool schema:

```console
$ curl -s localhost:53211/computer -d '{"action":"left_click","coordinate":[420,180]}'
{"ok":true}
$ curl -s localhost:53211/computer -d '{"action":"screenshot"}' | jq -r .image.data | base64 -d > screen.png
```

## Why

An agent running *on* a desktop machine fights that machine: the screen locks, the OS
asks for permissions it cannot click through, a screensaver eats the session. A VNC
viewer has none of those problems — it controls the machine from outside, through an
interface built for exactly that. bolide is the same trick with an HTTP front door
instead of a viewer window.

That makes the remote desktop a **resource**, not a place the agent has to live in. One
machine can expose several; an agent on a Linux box can drive a Mac.

## Install

```console
$ curl -fsSL https://raw.githubusercontent.com/jakequist/bolide/main/install.sh | sh
```

Prebuilt for macOS (Apple Silicon and Intel) and Linux (x86_64 and aarch64, static musl
builds that run on any distro). The script installs to `/usr/local/bin` when it can write
there and `~/.local/bin` otherwise. `BOLIDE_VERSION=0.1.0` pins a version and
`BOLIDE_INSTALL_DIR=DIR` picks the directory.

Without a pinned version, the script — and `bolide update` below — learns the latest
release from where `github.com/jakequist/bolide/releases/latest` redirects, not from the
GitHub API, whose 60-calls-an-hour limit for anonymous clients runs out behind a shared
IP.

From source, on any platform with a Rust toolchain:

```console
$ cargo install --git https://github.com/jakequist/bolide bolide-cli
```

### Updating

```console
$ bolide update --check     # installed and latest versions; changes nothing
$ bolide update             # replace this binary with the latest release
```

`update` downloads the release for this platform and renames it over the running
binary in one atomic step, so an interrupted update leaves the old binary in place. It
needs `curl` and `tar`, which macOS and nearly every Linux already have. A binary built
with `cargo build` is updated with `git pull` instead, and `update` says so.

## Try it against a real desktop

Any RFB server works. The easiest throwaway one:

```console
# Linux — a headless X server with VNC on :1 (port 5901)
$ Xvnc :1 -geometry 1280x800 -SecurityTypes None &
$ DISPLAY=:1 xterm &
$ bolide connect vnc://127.0.0.1:5901

# or share an existing X display
$ x11vnc -display :0 -localhost -nopw &
$ bolide connect vnc://127.0.0.1:5900

# macOS — System Settings → General → Sharing → Screen Sharing, then
$ bolide connect vnc://127.0.0.1 --password-file ~/.vnc-pass
```

Then `bolide screenshot --out /tmp/s.png` and look at it.

## Commands

| command | what it does |
|---|---|
| `bolide connect vnc://HOST[:PORT]` | connect and start the server (daemonises; `--foreground` to stay attached) |
| `bolide status` | what is connected, and where the server is listening |
| `bolide disconnect` | stop the server and close the RFB session |
| `bolide screenshot [--out FILE]` | the current screen as PNG (`--out -` for stdout, even on a terminal) |
| `bolide click X Y [--button left\|right\|middle]` | a click |
| `bolide move X Y` | move the pointer |
| `bolide type TEXT` | type literal text |
| `bolide key CHORD` | a key or chord — `Return`, `ctrl+c`, `cmd+space`, `Page_Down` |
| `bolide scroll X Y --dir up\|down [--amount N]` | wheel notches under the pointer |
| `bolide clipboard push [TEXT \| --stdin]` | set the remote clipboard |
| `bolide clipboard pull` | the last clipboard the desktop sent |
| `bolide update [--check]` | replace this binary with the latest release |
| `bolide --version` | the installed version |

`connect` options: `--username U`, `--password PW`, `--password-file F` (or the
`BOLIDE_PASSWORD` environment variable), `--listen ADDR`, `--token T`, `--foreground`.
Credentials can also ride in the URL: `vnc://jake:pw@mac01.local` (percent-encode an `@`,
`:` or `%` in them).

## Passwords

Four ways in, all supported: the URL (`vnc://user:pw@host`), `--password PW`,
`--password-file F` (read once, never stored), and the `BOLIDE_PASSWORD` environment
variable. The URL or the flag wins over the file, which wins over the variable; giving
both the URL's password and a password flag is an error rather than a guess.

A password on the command line is visible to other local users through `ps` and is kept
in your shell history. On a machine you share, the file or the variable is the better
choice — bolide leaves that decision to you.

Wherever it comes from, the password goes nowhere else. It never reaches the state file,
`/status` (which reports `host:port`, never the URL), a log line, an error message or any
`Debug` output. The background daemon `connect` starts receives it through its
environment, not its command line, so it is not in `ps` for the life of the session.
bolide's own state file holds the endpoint, the pid and the server's bearer token, at mode
0600.

The server binds `127.0.0.1` on a kernel-assigned port by default, because it hands its
caller full control of somebody's desktop. `--listen 0.0.0.0:8000` (or any address)
exposes it wherever you say, and `--token` adds a bearer check.

## The HTTP protocol

`POST /computer` takes the `computer` tool's input object verbatim —
`screenshot`, `left_click`, `right_click`, `middle_click`, `double_click`,
`triple_click`, `left_click_drag`, `mouse_move`, `type`, `key`, `scroll`,
`cursor_position`, `wait` — plus `GET /screenshot`, `GET`/`POST /clipboard`,
`GET /status` and `GET /healthz`. Full schema: [docs/protocol.md](docs/protocol.md).

## What v0 does not do

- No TLS, no VeNCrypt, no Apple ARD authentication (security types `None` and classic
  VNC auth only). bolide is meant to be pointed at a desktop you can already reach —
  over a tailnet, a VPN or an SSH tunnel.
- No video or streaming endpoint; screenshots are pull-only.
- No horizontal scrolling (RFB's wheel is buttons 4 and 5, and nothing else is
  standard), no `hold_key`, no `left_mouse_down`/`up`.
- Clipboard is latin-1, per baseline RFB; the Extended Clipboard pseudo-encoding is not
  implemented, so a character outside latin-1 becomes `?`.
- Reading the remote clipboard is a *cache* of what the desktop volunteered. RFB has no
  "what is on your clipboard" message.

## Releasing

Releases are cut by pushing a tag; nothing is published by hand.

```console
$ # bump [workspace.package] version in Cargo.toml to X.Y.Z, then
$ cargo check --workspace          # refreshes Cargo.lock
$ git commit -am "release: vX.Y.Z"
$ git tag vX.Y.Z && git push origin main vX.Y.Z
```

`.github/workflows/release.yml` refuses a tag that disagrees with `Cargo.toml`, runs the
full suite, builds `aarch64-apple-darwin`, `x86_64-apple-darwin`,
`x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl`, and attaches
`bolide-X.Y.Z-<target>.tar.gz` for each (one `bolide` binary inside), a `SHA256SUMS` and
`install.sh` to the GitHub release — the assets `install.sh` and `bolide update` both
download. `.github/workflows/ci.yml` runs the gate on Linux and on Apple Silicon macOS
for every push to `main` and every pull request.

## Repository

| crate | what |
|---|---|
| `bolide-rfb` | the RFB 3.8 client: handshake, VNC auth, Raw/CopyRect/ZRLE, keysyms |
| `bolide-testkit` | an in-process fake RFB server — every test runs offline |
| `bolide-server` | the loopback HTTP server |
| `bolide-cli` | the `bolide` binary |

[docs/architecture.md](docs/architecture.md) explains the layering and why the seams
are where they are. [CLAUDE.md](CLAUDE.md) is the working agreement for anyone — human
or agent — changing this code; the short version is **test first, always**.

## Status and home

v0, and young. The home is <https://github.com/jakequist/bolide>. bolide is developed
inside a parent monorepo as a self-contained Cargo workspace and published to that
repository with monosplice; it depends on nothing in the parent, so what you clone is
exactly what is built and tested there.

## Licence

MIT OR Apache-2.0, at your option.
