# CLAUDE.md — working on bolide

bolide connects to an RFB (VNC) desktop and exposes it over HTTP as a **computer-use**
endpoint, so an agent can drive somebody else's screen without living on that machine.
Read [README.md](README.md) first, then [docs/architecture.md](docs/architecture.md)
and [docs/protocol.md](docs/protocol.md).

This is a self-contained Cargo workspace. It is developed inside a parent monorepo and
published from there to <https://github.com/jakequist/bolide> with monosplice, and it
depends on nothing in that parent — no shared crates, no shared config, no imports
across the boundary, no paths that point outside this directory. Keep it that way:
every file here must make sense, unedited, as the root of its own repository.

## TDD is non-negotiable

Write the failing test first, then the implementation. If you find yourself writing
implementation without a test that demands it, stop.

**Fixing a bug is TDD too.** Write a test that reproduces it and fails for the *right
reason* (red), then fix it (green). The regression test is part of the fix — a bug that
shipped once had no test guarding it.

Three places tests live, cheapest first:

1. **Pure unit tests**, inline in `#[cfg(test)] mod tests`. Pixel formats, keysym
   tables, chord parsing, the action planner, the wire types, state-file paths. Most
   tests are here, and most logic should be arranged so that it *can* be.
2. **Client-against-a-scripted-stream**, in `bolide-rfb`. `connect_stream` takes any
   `AsyncRead + AsyncWrite`, so a test drives the whole client over
   `tokio::io::duplex` with hand-written server bytes. Handshake, auth failures,
   malformed rects, timeouts.
3. **End to end against `bolide-testkit`**, a real RFB server that happens to be
   in-process. Everything that needs both halves: decoding a scripted screen, seeing a
   click arrive as a PointerEvent, the HTTP acceptance test.

Hard rules:

- **No live network in tests.** Not to a VNC server, not to a package registry, not to
  anything. Every test must pass on a disconnected laptop.
- **No `sleep` as synchronisation.** Wait on an event or a channel with a timeout. A
  test that sleeps is a test that flakes on a loaded CI box and hides a race on a fast
  one.
- **Nondeterminism belongs behind a seam** with a deterministic fake, the way the
  stream and the clock do.

## The gate

```console
$ cargo fmt --all --check
$ cargo clippy --workspace --all-targets -- -D warnings
$ cargo test --workspace
```

All three, green, before anything lands. `-D warnings` is not negotiable either: a
warning nobody is forced to fix is a warning everybody scrolls past.

## Conventions

- **The protocol is written down once.** `bolide_rfb::proto` holds every RFB constant,
  every fixed-shape message and the VNC-auth transform, and `bolide-testkit` *depends on
  it* rather than restating it. This matters more than it looks: bolide writes both ends
  of this wire, and a fake with its own idea of the protocol would agree with the client
  and disagree with TigerVNC — a green, wrong test suite. Same rule for the HTTP wire:
  `bolide_server::wire` is the single statement, and `bolide-cli` compiles against it
  instead of defining its own request bodies.

  Where two statements are genuinely needed (the ZRLE *decoder* in `bolide-rfb` and the
  ZRLE *encoder* in `bolide-testkit` are inverses, not copies), exactly one of them
  carries the format description and the other points at it. Today that is
  `bolide_rfb::zrle`'s module docs.

- **Layering runs one way**: `bolide-rfb` → `bolide-server` → `bolide-cli`.
  `bolide-testkit` depends on `bolide-rfb` and is a **dev-dependency everywhere else** —
  it must never appear in a normal `[dependencies]` block, or the fake server ships in
  the binary.

- **The CLI is wiring.** Anything worth testing in `bolide-cli` lives in its library
  half (`src/lib.rs` and friends); `main.rs` parses arguments and prints. Same for the
  HTTP layer: decisions go in `plan()`, which is pure.

- **Secrets never widen.** A password enters through a file or an environment variable
  and leaves through the RFB handshake. It does not reach argv, the state file,
  `/status`, a log line, or a `Debug` impl — `bolide_rfb::Config` redacts it, and there
  is a test that says so. When you add a type that can hold one, add that test.

- **Say what the desktop actually told us.** An unpainted framebuffer is black at
  exactly screen dimensions and is indistinguishable from a real black screen; a
  clipboard read is a cache of what the server volunteered; `cursor_position` is bolide's
  own last PointerEvent, because RFB cannot ask. Where bolide does not know, the answer
  says so rather than guessing plausibly.

- **Errors are for the person reading them.** `Error::PasswordRequired` and
  `Error::AuthFailed` are separate variants because one is fixed by supplying a password
  and the other by supplying a different one. Keep that standard.

- **Docs are part of the deliverable.** A change that alters behaviour described in
  `README.md` or `docs/` updates it in the same commit.

## Releasing

Not yet. bolide has no published version, no tags and no crates.io presence; `version`
is `0.0.0` across the workspace. Once its own repository is live, releasing becomes
a tag plus `cargo publish` for the four crates in dependency order (`bolide-rfb`,
`bolide-testkit`, `bolide-server`, `bolide-cli`) — and this section gets replaced by the
real procedure rather than this promise.

## Where this lives

The source of truth is a directory inside a parent monorepo; the public repository at
<https://github.com/jakequist/bolide> is spliced out of it. The parent knows about bolide
in exactly two deliberately minimal ways: a CI lane that runs the gate above on any
change here, and an ignore entry that keeps its JavaScript tooling from looking in here.
Nothing else. If you find yourself wiring bolide into the parent any further — or
pointing at a parent path from in here — that is the signal to stop: the splice would
publish a dangling reference.

`rust-toolchain.toml` pins `channel = "stable"` — a **channel, not a version** — on
purpose: rustup then resolves it against whatever stable a CI image already carries and
downloads nothing, which is what keeps a read-only `RUSTUP_HOME` harmless. Pinning an
exact version would make every CI job a toolchain download, and the parent's CI asserts
the pin is still a channel.
