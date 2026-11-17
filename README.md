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
| `bolide drag X1 Y1 X2 Y2` | drag with the left button, paced so Finder takes it |
| `bolide move X Y` | move the pointer |
| `bolide type TEXT` | type literal text |
| `bolide key CHORD` | a key or chord — `Return`, `ctrl+c`, `cmd+space`, `Page_Down` |
| `bolide scroll X Y --dir up\|down [--amount N]` | wheel notches under the pointer |
| `bolide clipboard push [TEXT \| --stdin]` | set the remote clipboard |
| `bolide clipboard pull` | the last clipboard the desktop sent |
| `bolide update [--check]` | replace this binary with the latest release |
| `bolide --version` | the installed version |

`connect` options: `--username U`, `--password PW`, `--password-file F` (or the
`BOLIDE_PASSWORD` environment variable), `--auth auto|ard|vnc`, `--listen ADDR`,
`--token T`, `--foreground`, `--record FILE`, `--record-fps N` and `--action-log FILE`
(below), `--pointer direct|humanized`, `--typing direct|humanized` and
`--seed N` (below), and the timing knobs `--drag-steps N`, `--drag-step-ms MS`,
`--drag-dwell-ms MS` (how a direct drag walks) and `--settle-ms MS` (how long a
screenshot after input lets the app redraw). Credentials can also ride in the URL:
`vnc://jake:pw@mac01.local` (percent-encode an `@`, `:` or `%` in them).

## Moving the pointer like a person

By default the pointer is `direct`: it jumps to each point and presses at once, the
fastest way there is. `bolide connect --pointer humanized` moves it like a hand instead:

- a move follows a curved path, fastest in the middle, timed by Fitts's law (further
  takes longer), with a little tremor, and a long move overshoots and comes back;
- a click hovers before the press and holds the button down; a double or triple click
  has human gaps between its clicks, all at one spot, within a pixel of the point;
- a drag reaches the start, presses, carries the item more slowly along its own path,
  pauses, and releases on the target;
- a scroll moves to the point first, then sends its notches in runs that speed up and
  slow down, with a pause between runs.

Every move starts from where bolide last left the pointer (from the centre of the
screen before its first one), so the pointer never jumps. Each act takes longer: about
half a second for a click near the pointer, about a second for one across half the
screen, up to two seconds corner to corner. The drag flags above pace a direct drag only;
a humanized drag paces itself and ignores them. Typing and keys follow `--typing`, below.

The randomness comes from one seeded stream per session: `--seed N` makes a session's
pointer move the same way each time it does the same things. Without `--seed`, `connect`
picks one and prints it. The movement is a port of meteor's humanized interaction
(`desktops/interaction`), with its constants.

## Typing like a person

By default typing is `direct`: a `type` sends each character's key down and up at once,
with no wait between keys. `bolide connect --typing humanized` types like a person
instead. It covers `type` and `key` only; the pointer acts follow `--pointer`.

- the wait between keys is log-normal around 150 ms, at a pace that wanders from word to
  word; common letter pairs (`th`, `er`, …) come faster, a space and the key after it
  slower, and anything but a letter slower still; now and then (3% of keys) there is a
  pause of 300 to 900 ms;
- each key is held 70 to 130 ms, and a key may still be down when the next goes down, but
  only when neither needs a Shift and they are different keys, so the text cannot change;
- a `key` chord presses its modifiers a little apart, waits, holds the key, and lets the
  modifiers go in reverse.

The keys that arrive are the same as `direct`'s, one down and one up per character, so
the text is exactly the text asked for. The cost is time: about 5 characters a second,
so a 100-character line takes about 19.5 s (measured: the mean over 200 seeds of a
100-character line of prose is 19.5 s, 5.1 characters a second). A request returns
once its last key has gone out, so a client waits that long, and an action log's `type`
line spans it. `--seed N` covers typing too: the pointer and the typing draw from the
one stream, act by act, and with `--typing direct` a seed moves the pointer exactly as
it did before. The typing is a port of meteor's humanized typing, with its constants;
meteor's optional typos are not ported.

## Recording a session

`bolide connect vnc://HOST --record run.mp4` records the desktop for the whole session,
to a `.mp4` (H.264) or a `.webm` (VP8); the extension decides, and any other is a usage
error. It needs `ffmpeg` on PATH, and `connect` says so before it starts anything if there
is none. The recording starts with the session and is finalised when it ends: `bolide
disconnect`, Ctrl-C under `--foreground`, or the desktop going away. ffmpeg runs in a
process group of its own, so a Ctrl-C at the terminal reaches bolide, which then closes
the video properly.

- The video samples the framebuffer at 30 frames a second, repeating the last frame while
  nothing changes, so its length is the session's. `--record-fps N` sets another rate,
  from 1 to 60; anything else is a usage error. A humanized pointer move has 30 to 60
  positions, and a lower rate shows only some of them.
- Its size is the desktop's at connect, rounded up to even (H.264 cannot encode an odd
  side; the extra column or row is black). A desktop that resizes later is fitted into
  that size: centred unscaled if it fits, scaled down with its aspect kept if it does not.
- Until the desktop's first update arrives the framebuffer is black, and so is the video.
- The MP4 is fragmented, with a keyframe every two seconds, so a file cut off by a crash
  still plays up to its last two seconds. An existing file is overwritten.

## Logging what was done

`bolide connect vnc://HOST --action-log run.jsonl` writes a log of the session to
`run.jsonl`, as JSON Lines: one line for each request the server handles, so another tool
can find the stretches where nothing happened and cut them out of a recording. It works
with or without `--record`. An existing file is overwritten.

**The log holds typed text in full, passwords included.** A `type` action's text is
written as it was sent, so keep the file as private as what was typed.

The first line is a header:

```json
{"bolide":"0.2.0","desktop":{"width":1280,"height":800},"connected":"2026-11-17T10:00:00.120Z","record":{"file":"/runs/run.mp4","fps":30,"start":"2026-11-17T10:00:00.164Z"},"pointer":"humanized","typing":"direct"}
```

`record` is `null` without `--record`. Then one line for each `/computer` action,
`/screenshot`, and `/clipboard` read and write, as each is answered:

```json
{"seq":1,"action":"left_click","params":{"coordinate":[640,400]},"start":"2026-11-17T10:00:02.500Z","end":"2026-11-17T10:00:03.310Z","startS":2.336,"endS":3.146,"ok":true}
{"seq":2,"action":"type","params":{"text":"hello\n"},"start":"2026-11-17T10:00:03.400Z","end":"2026-11-17T10:00:03.520Z","startS":3.236,"endS":3.356,"ok":true}
{"seq":3,"action":"left_click","params":{"coordinate":[9000,1]},"start":"2026-11-17T10:00:04.000Z","end":"2026-11-17T10:00:04.000Z","startS":3.836,"endS":3.836,"ok":false,"error":"coordinate (9000, 1) is outside the 1280x800 screen"}
```

- `action` is the `/computer` action's name, or `screenshot`, `clipboard_get` or
  `clipboard_set`. A `/computer` body with no action name to read is `computer`.
- `params` is what was sent, less the action name: coordinates, text, keys, the
  screenshot's query (`{"format":"json"}`). A body that is not a JSON object is
  `{"body":"…"}`.
- `start` and `end` are UTC, to the millisecond: when bolide began on the request and
  when it had the answer. An act that sends input has its answer once its last pointer
  or key event has gone out to the desktop, so `end` is at or after it. `startS` and `endS` are the same times in seconds from the
  recording's first frame with `--record`, so they are positions in the video, and from
  the connect without it. All of them come from one monotonic clock, so a wall clock
  that is changed mid-session does not reorder them.
- `ok` is `true`, or `false` with `error`, the message the caller was answered with.
- `/status` and `/healthz` are not logged, nor is a request refused for its token.
- Lines are written in the order requests are answered. A request that overlaps another
  (a screenshot during a click) can start before the line above it ends.

Each line is flushed as it is written, so a crash keeps every line before it. If a line
cannot be written (a full disk), the request still goes through: the line is lost, the
first loss is said in the daemon's log, `bolide.log`, and the count when the session
ends. Lines keep their `seq` either way, so a gap in `seq` is where lines were lost. If
the file cannot be created at connect, `connect` fails.

## Authentication, and logging in as yourself on a Mac

bolide speaks three RFB security types: `None`, classic VNC auth (type 2), and **Apple
Remote Desktop (type 30)** — the Diffie-Hellman username/password exchange every macOS
Screen Sharing server offers. Which one it uses is `--auth`:

- `auto` (the default) picks type 30 when the server offers it **and** a `--username` is
  set, otherwise classic VNC auth, otherwise `None`.
- `ard` forces type 30; `vnc` forces classic VNC auth.

The difference matters on macOS. Classic VNC auth carries a **password only**, so it
authenticates to whatever shares the physical console — connect to a Mac somebody is
sitting at and you get *their* screen. Apple's type 30 carries a **username**, so it
logs in **as that user**: it reaches a Mac sitting at its login window, and names which
account the session belongs to.

```console
$ bolide connect vnc://mac01.local --username meteor --password meteor --auth ard
```

Newer macOS also offers SRP/RSA variants (types 33, 35, 36) and a post-authentication
*session-select* exchange (the "Share screen / Log in as yourself" picker in Apple's own
viewer). bolide implements neither yet; see [docs/protocol.md](docs/protocol.md).

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

- No TLS and no VeNCrypt. Authentication is `None`, classic VNC auth, or Apple's type 30
  (see above); the newer Apple SRP/RSA types (33, 35, 36) and the session-select
  exchange are not implemented. bolide is meant to be pointed at a desktop you can
  already reach — over a tailnet, a VPN or an SSH tunnel.
- No video or streaming endpoint; screenshots are pull-only. (`connect --record` writes
  the session to a local video file; nothing streams it.)
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
| `bolide-rfb` | the RFB 3.8 client: handshake, VNC + Apple (type 30) auth, Raw/CopyRect/ZRLE, keysyms |
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
