# Native playback endpoint (M10)

Status: **shipped** — the self-registering `native` player kind is complete and verified.
`crates/musicata-endpoint`. (The audio path's *refinements* — `wss`, gapless, seek-following —
remain; see Scope below.)

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

Later runs reuse the saved `musicata-endpoint.json` (a stable id + scoped token, so a relaunch
re-attaches to the *same* server-side player — it never creates a duplicate):

```sh
musicata-endpoint --server http://musicata.local:3030
```

Then pick "Living Room" as the output/zone in the web app and play — audio comes out of the
device. Transport (play/pause/next), queue changes, and autoplay all flow through because the
endpoint follows the server-owned queue and reports track-ends back.

### Run it as a service (headless device)

For a Raspberry Pi by the amp, configure it with environment variables instead of a long
command line (CLI flags still override): `MUSICATA_ENDPOINT_SERVER`,
`MUSICATA_ENDPOINT_USER_TOKEN`, `MUSICATA_ENDPOINT_NAME`, `MUSICATA_ENDPOINT_STATE` (the
credentials path). Ship **`packaging/musicata-endpoint.service`**: register once by hand
(creating the player + token under the unit's `StateDirectory`), then `systemctl enable --now`
runs it reusing those creds — no user token in the unit. The service user must be in the `audio`
group, and the unit deliberately omits `PrivateDevices` so it can open `/dev/snd`.

**Verified end-to-end** (server + the actual `musicata-endpoint` binary): `--register` creates
the `native` player and saves creds; a second launch with no `--register` reuses the same id
(the server player count stays 1 — no duplicate); and its scoped token authenticates its own
channel (`/api/players/{id}/state` → 200) while a wrong/absent token is rejected (401).

## Scope / limits (audio-path refinements)

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
