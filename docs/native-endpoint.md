# Native playback endpoint (M10)

Status: **prototype shipped** (2026-06-27). `crates/musicata-endpoint`.

A native endpoint is a small program a device (a Raspberry Pi by the speakers, a spare desktop)
runs to become a real Musicata player — not a browser tab, not MPD. The server drives it from
the same **server-owned queue** as every other player; the device just plays what it's told and
reports back.

## Why it's small on the server

The browser player already *is* this: a queue-output player that receives the queue + stream
URLs over a bidirectional WebSocket and reports `progress`/`ended` back. So a native endpoint
reuses it wholesale — `bring_up()` maps the `native` kind to a `BrowserPlayer` under its own id.
A native endpoint differs only in **identity**: it's a registered, removable,
token-authenticated player rather than the always-present browser singleton. All the queue,
zone, leveling, and command machinery is shared, which is exactly M10's "browser and endpoint
players share the same queue/zone command model."

## Architecture

```
            register (user token, once)         ┌─────────────────────────┐
  endpoint ───────────────────────────────────▶ │  POST /api/players       │
     │                                          │  {kind:native,           │
     │   ◀──────── player id + token ────────── │   issue_token:true}      │
     │                                          └─────────────────────────┘
     │   ws://…/api/players/{id}/ws?token=…   ┌──────────────────────────────┐
     ├──────────────────────────────────────▶ │ BrowserPlayer (server-owned  │
     │   ◀── PlaybackState (queue + URLs) ──── │  queue, bidirectional WS)    │
     │   ─── {type:progress}/{type:ended} ───▶ └──────────────────────────────┘
     │
     │   GET /api/tracks/{id}/stream  (Bearer = player token)
     └──────────────────────────────────────▶  audio bytes ──▶ rodio ──▶ speakers
```

The endpoint holds only a **scoped per-player token**, never a user account. That token
authenticates its WS channel *and* the audio streams it fetches (`require_auth` accepts a valid
player token for `/api/tracks/{id}/stream`). The control logic is a pure function
(`protocol::decide`: given the new `PlaybackState` and what we're doing now → load / pause /
resume / stop), kept separate from the rodio/IO side so it's unit-tested without a sound device.

## Build & run

The endpoint pulls in audio output (rodio → ALSA/CoreAudio/WASAPI), so it is **excluded from the
default workspace build** (`default-members` in the root `Cargo.toml`) — the server's
`cargo build`/`cargo test`/release path never needs audio libraries. Build it explicitly:

```sh
cargo build -p musicata-endpoint --release   # needs audio dev libs (e.g. libasound on Linux)
```

First run — register with your Musicata API token (Account → token); the credentials are saved:

```sh
musicata-endpoint --server http://musicata.local:3030 --register \
    --user-token <YOUR_API_TOKEN> --name "Living Room"
```

Later runs reuse the saved `musicata-endpoint.json`:

```sh
musicata-endpoint --server http://musicata.local:3030
```

Then pick "Living Room" as the output/zone in the web app and play — audio comes out of the
device. Transport (play/pause/next), queue changes, and autoplay all flow through because the
endpoint follows the server-owned queue and reports track-ends back.

## Scope / limits (it's a prototype)

- **`ws://` only** (LAN). Put it behind a TLS reverse proxy for `wss://`.
- **Whole-track buffering:** each track is downloaded to memory then decoded — simple, fine for
  the LAN, not optimized for huge files.
- **No precise seek-following:** a server-side seek on the same track isn't mirrored (rodio
  doesn't seek mid-stream easily). Track changes, pause/resume, and end-of-track all work.
- **No gapless / crossfade.**
- The audio path needs a real output device, so it's verified by **manual run** (like the
  `live_mpd` test); the server-side registration + stream auth and the client's decision logic
  are covered by automated tests.

## Tests

- Server: `native_endpoint_registers_and_streams_with_its_token` (registration + stream-token
  auth), plus the existing player-token channel tests.
- Client: `cargo test -p musicata-endpoint` — the `decide()` state machine and the URL/slug/arg
  helpers (no audio device needed to compile the tests on a machine with audio dev libs).
