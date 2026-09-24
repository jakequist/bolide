# bolide — the HTTP protocol

Everything bolide exposes, on loopback by default (`--listen` puts it anywhere), as JSON.

The action vocabulary is **1:1 with Anthropic's `computer` tool**: an agent that already
holds that tool's schema can POST the tool input object unchanged. That is the design
goal, and it is why the spelling here is the tool's and not a tidier one —
`left_click` rather than `click(button)`, `coordinate: [x, y]` rather than `{x, y}`,
`duration` in seconds for `wait` while everything else is milliseconds.

The authoritative statement of these shapes is `bolide_server::wire` in Rust. This page
describes the same wire for anyone writing a client in another language; where the two
disagree, the code is right.

## Base URL and auth

`bolide connect` prints the base URL and writes it to the state file. The default bind is
`127.0.0.1:0` — loopback, kernel-assigned port — because this API hands its caller full
control of somebody's desktop.

With `--token T`, every route except `GET /healthz` requires:

```
Authorization: Bearer T
```

A missing or wrong token is `401` with `"error": "unauthorized"`.

## `POST /computer`

Body: one action object. Response: `200` with `ComputerResponse`.

```json
{"ok": true}
```

| action | fields | notes |
|---|---|---|
| `screenshot` | — | returns `image` |
| `left_click` | `coordinate` | move, press button 1, release |
| `right_click` | `coordinate` | button 3 |
| `middle_click` | `coordinate` | button 2 |
| `double_click` | `coordinate` | one move, two press/release pairs |
| `triple_click` | `coordinate` | one move, three press/release pairs |
| `left_click_drag` | `start_coordinate`, `coordinate` | press at start, move, release at end |
| `mouse_move` | `coordinate` | |
| `type` | `text` | one key down/up per character; `\n` types `Return` |
| `key` | `text` | a chord: `Return`, `ctrl+c`, `cmd+space`, `Page_Down` |
| `scroll` | `coordinate`, `scroll_direction`, `scroll_amount` | `up` or `down` only |
| `cursor_position` | — | returns `coordinate` |
| `wait` | `duration` (seconds) | |

**One action at a time.** bolide holds the desktop for the whole length of an action, so
concurrent requests queue rather than interleaving. A desktop has one pointer and one
keyboard: without this a `type` racing a `click` reaches it braided, which is text and
clicks nobody asked for. Requests are not rejected, only serialized — but a caller that
fires several actions at once cannot predict their order, and should await each one.

`coordinate` is `[x, y]` in framebuffer pixels. A coordinate outside the screen is a
`400` with `"error": "out_of_bounds"` — bolide does not clamp, because a caller that
thinks the screen is bigger than it is needs to be told rather than quietly redirected
to the edge.

`scroll_amount` and `duration` are honoured in full — there is no cap on how far a caller
scrolls or how long it waits; a negative or NaN `duration` is zero. Request bodies have no
size limit either, so a large clipboard paste goes through.

### `screenshot`

```json
{
  "ok": true,
  "image": {"format": "png", "data": "<base64>", "width": 1728, "height": 1117}
}
```

bolide asks the desktop for a fresh frame first. If the framebuffer has never been
painted it requests a full update and waits; if that times out the answer is `504` with
`"error": "timeout"`, rather than a black image that looks like a real screen.

### `cursor_position`

```json
{"ok": true, "coordinate": [420, 180]}
```

This is **bolide's own last PointerEvent**, not a reading. RFB has no message that asks a
server where the cursor is — the protocol is write-only in that direction. Before
anything has moved the pointer it is `[0, 0]`.

## `GET /screenshot`

Raw `image/png` bytes. With `?format=json`, the same `image` object as above. Same
freshness rules.

## `GET /clipboard`

```json
{"text": "whatever the desktop last sent"}
```

`null` when the desktop has never sent one. This is a **cache of what the server
volunteered** through ServerCutText — RFB has no "what is on your clipboard" request, so
a caller that needs the current selection has to make the desktop copy something first.

## `POST /clipboard`

```json
{"text": "put this on the remote clipboard"}
```

Sent as RFB ClientCutText, which is **latin-1**: a character outside it becomes `?`.
(The Extended Clipboard pseudo-encoding that would fix this is not implemented.)

## `GET /status`

```json
{
  "connected": true,
  "remote": "mac01.local:5900",
  "desktop_name": "mac01",
  "width": 1728,
  "height": 1117,
  "painted": true,
  "version": "0.1.0"
}
```

`remote` never carries credentials.

## `GET /healthz`

`200 ok`, no auth, no body of interest. Exempt from the token so a supervisor can probe
without holding one.

## Errors

Every failure, at every route:

```json
{"ok": false, "error": "out_of_bounds", "message": "coordinate (9999, 10) is outside the 1728x1117 screen"}
```

| `error` | status | means |
|---|---|---|
| `bad_request` | 400 | the body did not parse, or a field was out of range |
| `out_of_bounds` | 400 | a coordinate is off-screen |
| `unknown_key` | 400 | bolide does not know that key name |
| `unauthorized` | 401 | `--token` is set and the request did not present it |
| `disconnected` | 503 | the RFB session is gone |
| `timeout` | 504 | the desktop did not produce a frame in time |
| `internal` | 500 | anything else |

Branch on `error`, not on `message`.

## Key names

The spellings `XStringToKeysym` accepts — the same strings `xdotool key` takes, which is
what the `computer` tool's `key` action already emits: `Return`, `Escape`, `Tab`,
`BackSpace`, `Delete`, `Page_Up`, `Page_Down`, `Home`, `End`, `Left`, `Up`, `Right`,
`Down`, `F1`–`F12`, `space`, and the modifiers `Shift_L`, `Control_L`, `Alt_L`,
`Meta_L`, `Super_L` (and their `_R` twins).

Resolution is case-insensitive and aliased, because models are not consistent: `enter`,
`esc`, `del`, `arrowleft`, `pageup`, `ctrl`, `control`, `alt`, `option`, `cmd`,
`command`, `win`, `super`, `meta`, `shift`. A bare modifier means the **left** one.

`cmd` resolves to `Meta_L`, which is what a Mac VNC server maps to Command — so
`cmd+space` opens Spotlight on the far end.

Chords split on `+` and press left to right, releasing in reverse, so `ctrl+shift+t`
holds both modifiers while `t` goes down. A literal plus is `plus`.
