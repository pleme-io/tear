# mado-web

Browser-side mado renderer skeleton. Attaches to `tear-daemon` via
`tear-ws-bridge` over WebSocket.

## What ships today

- **Wire foundation** — the wasm crate speaks the same typed CBOR
  `Request` / `Response` shape as `tear-client`, framed inside WS
  binary messages.
- **ListSessions probe** — on connect, sends `Request::ListSessions`
  and renders the result.
- **Pane subscribe + stream** — when a pane id is supplied, promotes
  the connection to `Subscribe(pane_id)` and appends incoming bytes
  to a `<pre>`.

## What's coming

- WebGPU cell-grid renderer (glyph atlas, per-cell instanced
  rectangles — same shape as native mado).
- Keyboard input → `send_keys` back over the wire.
- Multi-pane layout, scrollback, search.

## Quick test (today)

```bash
# 1. Start the daemon + bridge (fleet default if pleme.terminal.tear is on)
tear daemon &                      # ~/.local/share/tear/tear.sock
tear-ws-bridge --listen 127.0.0.1:8181 &

# 2. Build the wasm
cd mado-web
wasm-pack build --target web --out-dir pkg

# 3. Serve the static page
python3 -m http.server -d static 8000

# 4. Open http://localhost:8000/
#    Click "connect" — the sessions list populates.
#    Paste a 16-hex pane id, click "connect" again — bytes stream.
```

## Architecture

```
browser   ──ws──▶  tear-ws-bridge  ──UDS/TCP──▶  tear-daemon
mado-web                                          (InProcess)
(wasm32)                                          PaneGrid + PTY
```

The bridge speaks the CBOR wire to the daemon and forwards each
frame verbatim into a WS binary message. mado-web decodes the
length-prefixed CBOR frame inside the message and runs the same
`Response` matcher native `tear-client` does — no protocol
translation, no schema duplication.

## Not yet a workspace member

`mado-web` is currently **not** in the tear workspace `members =
[...]` list because building wasm targets in the normal workspace
test cycle would slow down CI for no benefit. Build it explicitly
via `wasm-pack build` from its own directory.
