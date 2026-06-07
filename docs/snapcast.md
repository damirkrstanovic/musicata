# Snapcast — reliable synchronized network transport (research + plan)

Date: 2026-06-07

## Context

Milestone 10 calls for non-browser player endpoints and "research Snapcast for synchronized
transport." This is that research, against the checked-out source at `../snapcast`. The goal:
**reliable audio transport from the Musicata server to networked players** — a dedicated,
low-maintenance endpoint (e.g. a Raspberry Pi by the speakers) that plays what the server
sends, robustly, and *optionally in sample-accurate sync* across rooms.

**The one decision that dominates the design:** Snapcast is a **broadcast of a continuous,
server-decoded PCM stream** — the opposite of Musicata's current "the endpoint fetches a
per-track URL and decodes it itself" model. Adopting Snapcast means Musicata must, for the
first time, **decode audio to PCM server-side and feed a continuous stream**. That's the real
cost; everything else is small. (It also dovetails with the server-side DSP tier — see below.)

**Recommendation:** Use the real **snapserver** binary as a managed subprocess (or
external service), feed it decoded PCM through a **FIFO**, and control it via its
**JSON-RPC** API. Do **not** reimplement the Snapcast protocol in Rust — its battle-tested
sync engine is the entire point. New Musicata work = a server-side decode→FIFO loop + a
JSON-RPC control client + a `Snapcast` player/zone variant. Cargo-feature-gated, "require
snapserver installed," like the SMB source.

---

## What Snapcast is, and why it fits "reliable transport"

A **snapserver** reads a continuous PCM stream from a source (a named FIFO is the usual
integration point), encodes it (FLAC default, or PCM/Opus/Vorbis), and broadcasts it over TCP
to N **snapclients** (lightweight C++ players that run on Pis, desktops, phones, or the
in-browser Snapweb client). Each client **buffers ~1 s and plays every chunk at a
server-stamped presentation time**, so all clients are sample-accurate to within a few ms on a
LAN. Control is a **JSON-RPC** API (groups/clients/streams). It is the mature open-source
equivalent of Sonos-style multi-room.

Its data model maps cleanly onto ours:

| Snapcast | Musicata |
|---|---|
| **stream** (a `source=` FIFO) | a zone's audio output |
| **group** (plays one stream) | a **zone** |
| **client** (a snapclient device, own volume + latency) | a player/output in the zone |

---

## How it stays reliable + in sync (the mechanism)

The crux, from `doc/binary_protocol.md` + `client/`:

- **Plain TCP** carries a small binary protocol (`common/message/`): `Hello`,
  `ServerSettings`, `CodecHeader` (lets a mid-stream joiner init its decoder), `WireChunk`
  (encoded audio + a **server-time presentation timestamp**), and `Time` (NTP-like).
- **Clock sync** (`client/time_provider.cpp`): each client measures its offset to server time
  via `Time` round-trips and takes a **median over ~200 samples** (50 rapid syncs on connect,
  then 1/s) → ~1 ms accuracy on a LAN. `serverNow() = localNow() + median_offset`.
- **Playout scheduling** (`client/stream.cpp`): a chunk stamped `T` is played when
  `serverNow() ≥ T + bufferMs`. Too old → dropped; too young → silence. `bufferMs` (default
  **1000 ms**, via `ServerSettings`) is the end-to-end latency and the single
  resilience-vs-latency knob.
- **Drift correction:** *soft sync* nudges playback rate by ≤±0.05% (transparent resample,
  drop/dup a frame) to track the server clock; *hard sync* (drop old chunks + inject silence)
  recovers from big jumps or a mid-stream join.
- **Failure modes:** TCP hides packet loss (retransmit; the buffer absorbs the stall). A real
  network drop → the client **reconnects after 1 s** and resyncs in ~200 ms (a brief dropout,
  but clients never permanently drift). Tuning `bufferMs` up buys resilience at the cost of
  latency.

**Bottom line:** sample-accurate, self-healing, auto-reconnecting transport over an
unreliable LAN — at a deliberate **~0.5–1 s latency** (fine for music; not for tight UI
feedback or A/V sync). Exactly the "reliable transport" property wanted.

---

## The integration: feed a FIFO, control over JSON-RPC

From the snapserver research (`server/streamreader/pipe_stream.cpp`, `doc/configuration.md`,
`doc/json_rpc_api/`):

- **Audio in — the pipe source.** `snapserver.conf`:
  `source = pipe:///run/musicata/snapfifo?name=Musicata&sampleformat=48000:16:2&codec=flac`.
  Musicata writes **raw interleaved PCM** at the declared format (48 kHz / 16-bit / stereo =
  4 bytes/frame; snapserver reads `chunk_ms`, default 20 ms ≈ 3840 B). On no-data the stream
  goes **idle**; on EOF snapserver waits and reconnects to the FIFO. `mode=read` lets Musicata
  own FIFO creation.
- **Control — JSON-RPC** over TCP **1705** (newline-delimited) or HTTP/WebSocket **1780**.
  Key calls Musicata makes: `Server.GetStatus` (discover groups/clients/streams),
  `Group.SetStream(group, "Musicata")` (point a zone at our stream), `Group.SetClients`
  (move a device between zones), `Client.SetVolume` / `Client.SetLatency` (per-output volume
  + delay). Subscribe (WS) to `Client.OnConnect/OnDisconnect/OnVolumeChanged`,
  `Group.OnStreamChanged`, `Server.OnUpdate` to keep our state live.
- **Snapweb** (the bundled web client) and real **snapclient** binaries are the players — no
  client code for us to write.

---

## The architectural mismatch + the new machinery

Today (`players.rs`, `main.rs:stream_track`): **endpoints pull per-track URLs and decode.**
The browser `<audio>` fetches `/api/tracks/{id}/stream` (byte-range, encoded bytes); MPD is
told to fetch the same URLs. **Musicata never decodes audio for playback** — symphonia exists
but only in `fingerprint.rs` (first ~120 s for AcoustID).

Snapcast needs the opposite: **one continuous, gapless, server-decoded PCM stream per zone.**
So the new pieces are:

1. **A per-zone decode loop** (new `snapcast.rs`): walk the zone's canonical `QueueState`,
   decode each track to PCM with **symphonia** (reuse the `fingerprint.rs:decode_samples`
   pattern, but full-length + streaming), and **write PCM to the zone's FIFO**, pacing to real
   time. Respond to queue/seek/skip/pause by repositioning the decoder — the loop *is* the
   playback cursor (like MPD's idle loop reconciles, but here we own the samples).
2. **Gapless concatenation + sample-rate handling.** snapserver runs at a fixed rate
   (48 kHz); the library is mixed (44.1/48/96). Decode loop **resamples to the zone rate**
   (rubato) and concatenates tracks with no silence.
3. **FIFO + subprocess management.** Create/own the FIFO; manage the `snapserver` process
   (Musicata manages no subprocess today — this is new, mirrors the planned CamillaDSP
   management); handle writer-blocking when no client is listening.
4. **A `Snapcast` player/zone variant** in the `PlayerHandle` enum (`players.rs`) — one
   variant + match arms, cargo-feature-gated. Registered/persisted like MPD (the `players`
   table); controlled by translating `PlayerCommand`s into decode-loop repositioning +
   JSON-RPC (volume/grouping). Slots into `ZonePlayer` member-driving.

**This is where real per-zone synchronized output finally exists** — the roadmap's deferred
"no audio sample-sync" item (`players.rs`) is solved *by Snapcast* for Snapcast zones, rather
than by us building clock-sync from scratch.

### Synergy with the DSP tier

The decode→FIFO path is the natural home for **server-side per-zone DSP** (see `docs/dsp.md`
Tier 2). Once Musicata decodes to PCM per zone, it can apply that zone's EQ/room-correction
profile *before* the FIFO — either with the vendored biquad/FIR math or by routing through
CamillaDSP — giving corrected, synchronized multi-room audio from one pipeline. Snapcast and
the DSP "Speakers" path share the same server-side PCM stage.

---

## Tradeoffs — when this is worth it

- **Pro:** rock-solid, self-healing, auto-reconnecting transport to dedicated endpoints;
  true sample-accurate multi-room sync; per-client volume/latency; reuses a mature engine; a
  snapclient on a Pi is a far more reliable endpoint than a browser tab.
- **Con / cost:** introduces **server-side decoding** (CPU per active zone; deliberately
  avoided so far), **~0.5–1 s latency**, a **broadcast model** (seek/skip/gapless become the
  decode loop's job, not a URL fetch), **subprocess management**, and a fixed zone sample
  rate. It's a genuinely new subsystem, not a small adapter.
- **Scope to single vs. multi:** even for *one* endpoint, Snapcast buys reliability +
  buffering + a clean dedicated player. The sync is a bonus that comes free once the
  decode→FIFO stage exists.

**Verdict:** the right tool for reliable + synchronized network playback to non-browser
endpoints, and the cleanest path to real zone sync. Gate it behind a cargo feature and
"requires snapserver," like SMB. Build the **server-side decode→FIFO stage once** — it serves
both Snapcast transport and the server-side DSP tier.

---

## Phased plan (sketch — not committed)

1. **Spike the decode→FIFO stage.** symphonia decode of the current queue → resample to
   48 kHz → write a FIFO, paced to real time, gapless. Verify with a hand-run snapserver +
   one snapclient. (The hard, novel part — do it first, in isolation.)
2. **`SnapcastPlayer`/zone variant.** Wire the decode loop to a zone's `QueueState`; translate
   play/pause/seek/next into decoder repositioning; persist + register like MPD.
3. **JSON-RPC control client.** Discover groups/clients; map zone membership + per-output
   volume to `Group.SetClients` / `Client.SetVolume`; subscribe to notifications for live
   state. snapserver lifecycle management (managed subprocess or external).
4. **`/admin` + settings.** Configure the snapserver endpoint, expose discovered
   clients as assignable zone outputs. Per "configuration lives in the product."
5. **Server-side DSP hook (optional, ties to `docs/dsp.md`).** Apply the zone's DSP profile in
   the decode path before the FIFO.

### Verification

A loopback integration test: spawn snapserver (FIFO source) + a snapclient writing to a file
sink; have Musicata decode a `testdata/` track into the FIFO; assert the client receives audio
and `Server.GetStatus` reflects the stream playing. Plus unit tests for the decode/resample/
gapless concatenation and the JSON-RPC client (against recorded responses).

---

## Sources

Snapcast source `../snapcast`: `doc/binary_protocol.md`, `doc/configuration.md`,
`doc/json_rpc_api/control.md`, `server/streamreader/pipe_stream.cpp`,
`common/sample_format.*`, `common/message/*`, `client/time_provider.*`, `client/stream.cpp`,
`client/controller.cpp`, `server/etc/snapserver.conf`. Musicata: `crates/musicata-server/src/
players.rs` (PlayerHandle, ZonePlayer), `main.rs` (stream_track, player WS),
`fingerprint.rs` (symphonia decode pattern).
