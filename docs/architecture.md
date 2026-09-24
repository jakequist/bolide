# bolide — architecture

```text
   an agent                    you, at a terminal
      │                                │
      │ HTTP/JSON                      │ bolide screenshot / click / type
      ▼                                ▼
 ┌──────────────────────────────────────────────┐
 │  bolide-server        127.0.0.1:<port>        │   ← same code path for both
 │    POST /computer   GET /screenshot   …      │
 └───────────────────────┬──────────────────────┘
                         │ bolide_rfb::Session (a trait)
 ┌───────────────────────▼──────────────────────┐
 │  bolide-rfb           one background task     │
 │    handshake · auth · decoders · framebuffer │
 └───────────────────────┬──────────────────────┘
                         │ RFB 3.8 over TCP
                         ▼
                  somebody's desktop
```

Four crates, one direction of dependency.

| crate | owns | depends on |
|---|---|---|
| `bolide-rfb` | the RFB protocol, the framebuffer, keysyms | — |
| `bolide-testkit` | a real RFB server that lives in the test process | `bolide-rfb` |
| `bolide-server` | the HTTP surface and the action planner | `bolide-rfb` |
| `bolide-cli` | the `bolide` binary: connect, daemonise, drive | `bolide-rfb`, `bolide-server` |

`bolide-testkit` is a **dev-dependency everywhere else**. It depends on `bolide-rfb`
rather than the other way round, so there is no cycle and the fake never ships in a
binary.

## The session is an actor

`bolide_rfb::connect` completes the handshake and then spawns one task that owns the
socket, the framebuffer and the decoder state for the life of the connection.
Everything else holds a `SessionHandle` — cheap to clone, `Send`, and a channel to that
task.

That shape is forced by the job. A computer-use server does two things at once: answer
an HTTP request, and keep the screen current. If the framebuffer were behind a lock that
an HTTP handler took, a slow screen would stall the API, and if the socket were read
only when somebody asked for a screenshot, a ServerCutText would sit unread for minutes.
So the read loop never stops, `ServerEvent`s fan out over a broadcast channel, and a
handler's "give me the current screen" is a message, not a borrow.

One action at a time. The HTTP layer holds the session for the whole length of an
action, because a desktop has one pointer and one keyboard: two `/computer` requests
running concurrently otherwise reach it braided together, and a `type "aaaa"` racing a
`type "bbbb"` types `abbabab a` into whatever had focus. Nothing below the HTTP layer
prevents that — the session's queue preserves the order commands *arrive* in, not the
order a caller meant them in — so the lock lives with the handler, and the cursor record
lives inside it.

Two more consequences worth naming:

- **A frame can be black and wrong.** RFB allocates the framebuffer at ServerInit and
  paints it later, so between connecting and the first update `framebuffer()` returns
  black at exactly screen dimensions. That is indistinguishable from a desktop showing a
  black screen, and a model handed one reports the app as blank. `Session::painted()`
  is the flag, and `/screenshot` waits on it.
- **Death is an event, not a hang.** When the socket dies the task emits
  `ServerEvent::Disconnected` and every later command returns `Error::Closed`
  immediately. The failure a daemon must never have is the silent one.

## The seams, and why each exists

**`connect_stream(impl AsyncRead + AsyncWrite)`.** The whole client can be driven over
`tokio::io::duplex` against hand-written server bytes. Handshake ordering, a failed
SecurityResult, a truncated rect, a server that stops mid-message — all unit tests, no
sockets.

**`bolide_rfb::Session` is a trait, and `bolide-server` takes `Arc<dyn Session>`.** Every
HTTP handler is tested against a recording fake session: no RFB, no fake server, no
timing. What a click *becomes* is asserted directly.

**`plan(action, ctx) -> Vec<RfbOp>` is pure.** The interesting questions about an action
are questions about a sequence: does a click move before it presses (it must — RFB has
no "click at (x, y)", so a press carries whatever position preceded it)? does a drag
release at the end coordinate? does a scroll of zero notches move the pointer at all
(it must not)? Those are table tests over a function, not integration tests.

**`bolide-testkit` is a real server.** Once the pure parts are covered, what is left is
whether bolide and a genuine RFB peer agree. The fake speaks the protocol over a real
loopback socket, shows a scripted screen, and records every event the client sends — so
"the click arrived at (420, 180)" is an assertion on a log, and the whole acceptance
test runs offline in milliseconds.

## Encodings

bolide advertises `[ZRLE, CopyRect, Raw, DesktopSize]`.

**Raw** is the floor: RFB requires every server to implement it, so it is never removed
from the list.

**CopyRect** costs four `u16` for a region the server already sent — a scroll, a window
drag, a menu closing. The one thing it must get right is overlap, since a scroll *is*
the overlapping case.

**ZRLE** is the compact one, and the choice deserves its reasoning. The alternative was
Hextile, which is simpler — 16×16 tiles, no shared state — but does not compress: its
tiles are raw or crudely run-coded and nothing is deflated. ZRLE runs palette and RLE
coding and *then* zlib, which is what makes "screenshot the whole desktop" cheap enough
to do on every model turn — the single operation bolide exists to serve. Every mainstream
server speaks it (TigerVNC, x11vnc, RealVNC, Apple Screen Sharing). The price is one
piece of shared state done right: **a single zlib stream per connection**, never reset
between rects. That is the bug the design invites, so there is a test for exactly it.

**DesktopSize** is a pseudo-encoding: advertising it asks the server to *tell* bolide
about a resize instead of dropping the connection.

The format itself is written down in one place — `bolide_rfb::zrle`'s module docs — and
the testkit's encoder points at it rather than restating it.

## The CLI and the daemon

`bolide connect` is the only command that speaks RFB. Almost everything else is an HTTP
call to the already-running server, located through a state file. That is deliberate:
`bolide screenshot` and an agent's `POST /computer` are then the *same* code path, so the
CLI cannot drift from the API or quietly work when the API is broken.

`bolide disconnect` is the exception, and for a reason worth keeping: there is no
shutdown route. An agent holding the endpoint can drive the desktop but must not be able
to end the session — the person who started bolide decides that — so `disconnect` signals
the pid from the state file instead.

Without `--foreground`, `connect` re-execs itself detached and **waits for the child to
write the state file** before printing the endpoint. Waiting is the point: a `connect`
that returned before the server was listening would make `bolide connect && bolide
screenshot` a race, and that is the first thing anybody types.

The state file (`$XDG_STATE_HOME/bolide/session.json`, or
`~/Library/Application Support/bolide/` on macOS, mode 0600) holds the endpoint, the pid,
the remote and the bearer token. It never holds a password. A file whose pid is gone is
treated as absent and replaced, so a crashed daemon does not leave every later command
timing out against a dead port.

## Portability

Linux and macOS, from one source tree. The only platform-specific decision is where
state lives, and it is a pure function of a platform tag and an environment lookup —
which means the macOS path is tested on Linux and vice versa, rather than a `#[cfg]`
that only runs where it already worked.
