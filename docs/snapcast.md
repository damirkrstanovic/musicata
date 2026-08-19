# Snapcast — reliable synchronized network transport (research + plan)

Date: 2026-06-07

> **Status — shipped.** Synchronized multi-room playout runs through `src/snapcast/`
> (feature-gated on `snapcast`, needs `snapserver`/`snapclient` at runtime), configured
> from /admin. Read the rest as the design behind what shipped; roadmap M10/M11 track
> what's left.

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

## Setting it up (user guide)

Musicata plays the same music **perfectly in sync across rooms**. A zone's queue is decoded to
PCM on the server and streamed through a managed `snapserver` to lightweight `snapclient`
players — a Raspberry Pi by the speakers, a desktop, a phone, or the in-browser Snapweb client.
Each client buffers ~1 s and plays on a server-stamped timestamp, so rooms stay sample-accurate
(at a deliberate ~0.5–1 s latency — great for music, not for tight A/V sync). This is also how
server-side per-zone DSP is applied (see [dsp.md](dsp.md)).

**Prerequisites** (Snapcast is built into the default binary, but the daemons are not bundled):

- `snapserver` on the Musicata host.
- `snapclient` on each playback device (or use the Snapweb browser client snapserver serves on
  port 1780).
- Optional cast-*in*: `shairport-sync` (AirPlay) and/or `librespot` (Spotify Connect) on the
  host — snapserver exposes each as an input you can play to all rooms.
- **In Docker:** the slim image does **not** include `snapserver`; add it in a derived image
  (`apt-get install snapserver`) or run snapserver on the host and point Musicata at it.

**Set it up** — all in the web UI, no flags or config files:

1. **/admin → Multi-room (Snapcast)**: toggle it on and set the server host (the address
   devices reach this machine at, e.g. `musicata.local`).
2. Under **Rooms**, name a room (e.g. `kitchen`); Musicata generates it and shows the exact
   `snapclient` command to run on that device. Repeat per device.
3. Pick which input the rooms play, and set per-room volume live. To stream from a phone
   instead, enable the AirPlay/Spotify inputs and cast to "Musicata".

**Security:** snapserver 0.35 does **not** enforce the per-room passwords yet (auth is stubbed
upstream), so multi-room is **LAN-only** — keep the server on a trusted network. Musicata writes
forward-compatible auth config that starts enforcing the moment a future snapserver enables it.

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

## Security / endpoint auth — per-room passwords (forward-compat) + room provisioning

**Verified-the-hard-way fact:** Snapcast 0.35's **username/password auth is stubbed out
upstream** — `snapserver.cpp` hardcodes `settings.auth.enabled = false; // TODO: auth`,
overriding any `[authorization]` config. The whole framework (the Hello `Auth` field,
`Server.Authenticate`, roles/users, per-method permissions) is *present but inert*: a client
sending an **empty** password connects fine (confirmed live). The only access control that
actually *enforces* on 0.35 is **mutual TLS** (`verify_clients` + client certs over `wss`),
which works but is a meaningful lift (a CA + per-device certs, `wss` clients) for a home setup
— so it's **not** what we ship here.

What Musicata implements (`server.rs`, gated by `auth_enabled`):

- **Per-room passwords + provisioning.** `/admin` → Multi-room → *Rooms*: name a room
  ("kitchen"), Musicata generates a 128-bit password, and gives you the ready
  `snapclient 'tcp://kitchen:<pw>@<host>:1704' --hostID 'kitchen'` command to run on the
  device. Rooms persist as JSON (`snapcast.rooms`); each becomes an `authorization.user` entry.
  `--hostID` makes the room show its connection state back in `/admin`.
- **Forward-compatible config.** When `auth_enabled`, we write the `[authorization]` block
  (`enabled = true`, `role = stream:Streaming`, one `user = <name>:<pw>:stream` per room). On
  0.35 it does nothing; the moment a future snapserver flips `auth.enabled`, the *same* config +
  passwords start enforcing with zero Musicata changes. Room/auth changes are written at
  startup, so they apply on restart.

**This is honest, not enforcing-today.** The `/admin` toggle carries a loud banner: the
passwords are *not* checked by snapserver 0.35, so rooms stay open on the LAN — keep the server
on a trusted network. We deliberately don't dress inert config up as working auth. The
provisioning interface (the install command per room) is useful regardless. (We prototyped the
TLS-cert path and reverted it: too complex for the gain on a home LAN; the password plumbing is
the cheaper bet that turns real on an upstream update.)

Future: when upstream finishes auth this becomes enforcing for free; a per-room TLS option can
return if someone needs real isolation before then.

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

## Implementation status — DONE (Phases 0–4), tested against snapserver 0.35

Built and verified end-to-end with the real `snapserver`/`snapclient` 0.35:

- **Phase 0** `crate::snapcast::decode` — symphonia full-length decode → stereo downmix →
  `rubato` FFT resample to 48 kHz → interleaved `i16`. Unit-tested.
- **Phase 0** `crate::snapcast::writer` — a dedicated OS thread streaming PCM into the FIFO,
  **self-paced to real time** (see hard-won note below).
- **Phase 1** `players.rs::SnapcastPlayer` (+ `PlayerHandle::Snapcast`) — server-owned queue,
  the decode loop is the playback cursor; gapless preload; registered/persisted like MPD as
  the always-present `snapcast-local` ("Multi-room (Snapcast)") player; drivable as a zone
  member via `drive_members`.
- **Phase 2** `crate::snapcast::{server,control}` — managed `snapserver` subprocess + FIFO
  lifecycle (one "Musicata" pipe stream); JSON-RPC client (enumerate clients, per-room volume,
  point groups at our stream).
- **Phase 3** `/api/snapcast/*` + `SnapcastPanel.svelte` — enable toggle + connected-room list
  with per-room volume, under `/admin` → Playback (configuration lives in the product).
- **Phase 4** `players.rs::leveling_gain` — per-track EBU R128 leveling applied in the writer
  (the server-side equivalent of the browser Track-mode gain), plus a live master volume.

**Verification.** Decode/resample unit tests + `next_index`/`leveling_gain` tests. Live: two
`snapclient`s capturing the stream were **100 % sample-identical at a sub-millisecond offset**
(0–3 frames = capture-process start jitter), proving synchronized multi-room. Transport
(play/pause/seek/next, elapsed) verified through the player command API.

### Hard-won notes (real bugs found while testing — don't regress these)

1. **The writer must self-pace to real time.** snapserver never backpressures our pipe writes
   here — it reads the pipe at real time and relays to each snapclient (which buffers ~1 s) —
   so a writer with no pacing spins a CPU and races the queue forward. `writer.rs` keeps a
   playout clock at most `PACING_LEAD` (1 s) ahead of now and sleeps: it bursts up front to
   prime the client buffer, then feeds at real time, and the lead doubles as the runaway cap
   when no client is connected.
2. **Feed in snapserver-sized chunks.** snapserver reads its pipe in small chunks paced to real
   time (its `chunk_ms`, default 20 ms). Writing coarse chunks (the first cut used 85 ms) makes
   the pipe sawtooth; matching ~20 ms (`CHUNK_FRAMES = 960`) keeps its input smooth and makes
   seek/skip/pause react within a chunk.
3. **Manage snapserver with `std::process`, not `tokio::process`.** An unwaited
   `tokio::process::Child` makes the runtime's SIGCHLD reaper busy-loop (≈300 % CPU at idle,
   observed). snapserver is a fire-and-forget long-lived child we never await output from, so
   `server.rs` spawns it with `std::process` and kills+reaps it explicitly on shutdown/drop.
4. **Tie the snapserver child to our lifetime with `PR_SET_PDEATHSIG`.** If Musicata dies
   *without* running Drop (crash/SIGKILL), the managed snapserver would orphan — and a
   restarted instance starting a *second* snapserver against the same FIFO means **two readers
   splitting the byte stream**, so both get a corrupt, gappy feed. `server.rs` sets
   `PR_SET_PDEATHSIG(SIGKILL)` via `pre_exec` so the kernel kills snapserver when we die.
   (This was the actual cause of "gappy audio" in testing — accumulated orphan snapservers all
   draining one FIFO; verified fixed by killing Musicata with `-9` and watching snapserver exit.)

(Also: in **debug** builds rubato's FFT resample of a multi-minute track takes several seconds,
so the first `play` is slow and `execute` blocks on it; release is < 1 s. A future refinement
is to decode off the command's response path.)

### How it was verified

An audio-truth harness generated tracks as distinct pure tones (and a noise track), at mixed
44.1/48 kHz to exercise the resampler, then captured the live `snapclient` stream and
**FFT-detected the playing frequency** to confirm the *audible* track matched the API state
(not just the state). 24 checks pass: play, correct pitch on resampled tracks, pause→silence,
next/prev/index, gapless auto-advance (no silence at the boundary), repeat one/off, enqueue,
clear, master-volume scaling, and two snapclients **100 % sample-identical at < 1 ms offset**.
Plus: 80 rapid mixed commands with no hang/deadlock, JSON-RPC per-room volume round-trip, and
~0 CPU at idle and during playback.

## Committed phased plan (multi-room synchronized playback)

This is the committed build order for milestone 10's "in-sync playback across two or more
zones." It is file-grounded against today's code; each phase is independently shippable and
the order is by **risk** (the novel decode stage first). The whole subsystem is gated behind a
new `snapcast` **cargo feature** and "requires snapserver installed," exactly like the `smb`
source.

**The sync question is settled:** we do **not** build clock-sync / drift-correction ourselves —
Snapcast's engine (above) provides it. The only *new* "advanced processing" we own is the
**server-side decode→PCM→FIFO** stage; Snapcast does the rest. So Phase 0 is the real work.

**New dependency:** `rubato` (MIT — license-compatible per AGPL convention) for sample-rate
conversion, feature-gated under `snapcast`. `symphonia` is already a dep (used by
`fingerprint.rs` + loudness), so decoding reuses an in-tree pattern.

### Phase 0 — Spike the decode→FIFO stage *(the hard, novel part — in isolation)*

The one thing Musicata has never done: decode audio to PCM server-side and feed a continuous
stream. Build it standalone before touching the player model.

- New module `crates/musicata-server/src/snapcast/decode.rs`. Reuse the
  `fingerprint.rs:decode_samples` pattern (symphonia probe → decoder loop →
  `SampleBuffer` interleaved) but **full-length and streaming** (don't cap at 120 s, don't
  buffer the whole track) — emit interleaved PCM at the declared zone format (48 kHz / 16-bit /
  stereo = 4 B/frame).
- **Resample** each track to the fixed zone rate with `rubato` (library is mixed 44.1/48/96).
- **Pace to real time + gapless:** write `chunk_ms` (~20 ms ≈ 3840 B) per tick; concatenate
  the next queue track with no intervening silence. Rely on the FIFO write blocking when no
  client drains it, plus a pacing sleep to bound buffering.
- **Verify (manual):** hand-run `snapserver` with
  `source = pipe:///run/musicata/snapfifo?name=Musicata&sampleformat=48000:16:2&codec=flac`
  + one `snapclient`; decode a `testdata/` track into the FIFO; confirm audio plays.

### Phase 1 — `Snapcast` player/zone variant

- Add `Snapcast(Arc<SnapcastPlayer>)` to the `PlayerHandle` enum (`players.rs:66`) + match arms
  in `is_online`/`subscribe`/`state`/`execute`.
- `SnapcastPlayer` owns the Phase-0 decode loop bound to a zone's canonical `QueueState`
  (`players.rs:1462`/`ZonePlayer`). **The decode loop is the playback cursor** — play/pause/
  seek/next reposition the decoder (analogous to how `ZonePlayer::execute` /`drive_members`
  reconcile members today, but here we own the samples).
- Register/persist like MPD: a `"snapcast"` kind arm in `PlayerHandlers::bring_up`
  (`players.rs:431`) spawns the decode task (mirroring MPD's task/poll handles); persisted in
  the existing `players` table.
- **Done when:** an MPD-less zone with a Snapcast output produces real synchronized audio —
  closing the deferred "no audio sample-sync" gap for Snapcast zones.

### Phase 2 — JSON-RPC control client + snapserver lifecycle

- New `crates/musicata-server/src/snapcast/control.rs`: a small JSON-RPC client over TCP 1705
  (newline-delimited) or WS 1780. Calls: `Server.GetStatus`, `Group.SetStream`,
  `Group.SetClients`, `Client.SetVolume`/`Client.SetLatency`. Subscribe to
  `Client.OnConnect/OnDisconnect/OnVolumeChanged`, `Group.OnStreamChanged`, `Server.OnUpdate`
  to keep state live.
- **snapserver lifecycle:** manage it as a subprocess via `std::process::Command` (Musicata
  manages **no** subprocess today — genuinely new; mirrors the planned CamillaDSP management in
  `docs/dsp.md`), owning FIFO creation (`mode=read`). External/already-running snapserver also
  supported.
- Map zone membership + per-output volume onto `Group.SetClients` / `Client.SetVolume`.
- **Decoupling:** each active Snapcast zone is its **own decode task draining its own queue at
  its own pace** — coordinating only through the DB — per the "decouple background operations"
  convention (the `*_loop` fns in `main.rs`).

### Phase 3 — `/admin` + settings *(configuration lives in the product)*

- Per the project convention (settings in the DB + web UI, **not** flags): a `/admin` panel to
  configure the snapserver endpoint and expose discovered snapclients as assignable zone
  outputs, edited live. No `--flag`.

### Phase 4 — Server-side DSP / loudness hook *(optional; ties `docs/dsp.md` + `docs/loudness.md`)*

- Apply the zone's DSP profile + EBU R128 leveling gain in the decode path **before** the FIFO.
  This is the already-noted "server-side apply for Snapcast" loudness item and the shared
  server-side PCM stage DSP Tier 2 wants — corrected, synchronized multi-room from one pipeline.

### Verification

A loopback integration test: spawn snapserver (FIFO source) + a snapclient writing to a file
sink; have Musicata decode a `testdata/` track into the FIFO; assert the client receives audio
and `Server.GetStatus` reflects the stream playing. Plus unit tests for the decode/resample/
gapless concatenation and the JSON-RPC client (against recorded responses). `cargo test`
default features must stay green with `snapcast` off; gated tests require an installed
snapserver and run under the feature.

---

## Sources

Snapcast source `../snapcast`: `doc/binary_protocol.md`, `doc/configuration.md`,
`doc/json_rpc_api/control.md`, `server/streamreader/pipe_stream.cpp`,
`common/sample_format.*`, `common/message/*`, `client/time_provider.*`, `client/stream.cpp`,
`client/controller.cpp`, `server/etc/snapserver.conf`. Musicata: `crates/musicata-server/src/
players.rs` (PlayerHandle, ZonePlayer), `main.rs` (stream_track, player WS),
`fingerprint.rs` (symphonia decode pattern).
