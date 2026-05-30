# Provider & Plugin System — Research and Plan

Background for Milestones 9 (music providers) and 10 (player providers/endpoints).
We studied Roon for the architecture, then chose an open, in-process model matching
how comparable open-source servers do it.

## How Roon does it (research)

Roon splits into three roles: **Core** (the brain — library, metadata, queue, DSP,
zone coordination), **Control** (thin remotes), and **Output** (endpoints that render
audio). It exposes **two unrelated extensibility surfaces**, plus a closed source
layer:

1. **RAAT / Roon Ready — the audio-endpoint layer.** RAAT ("AirPlay for audiophiles")
   tunnels audio Core→endpoint with the **device owning the audio clock**, ~<1 ms
   multi-room sync, exclusive/bit-perfect output, two-way volume + artwork. Integrated
   via a **closed, partner-only Roon Ready SDK** (free certification, NDA). Devices
   without RAAT use **Roon Tested** — bridged over AirPlay / Chromecast / USB.
2. **Extension SDK (`node-roon-api`) — the control-plane layer.** Out-of-process
   Node.js peers over a WebSocket RPC (MOO), discovered via SOOD. Extensions *consume*
   Transport/Browse/Image/Settings and *provide* Status/Settings/VolumeControl/
   SourceControl. It is explicitly **control only — no hook to add a music source or
   inject audio.**
3. **Music sources are closed/first-party** (TIDAL, Qobuz, KKBOX, nugs, radio). There
   is no public API to add a source — Roon's biggest self-acknowledged limitation.

**Design rules worth taking:** Core stays authoritative; capability negotiation
(providers declare what they support); strict separation of *source layer* vs
*transport layer*; automatic discovery + explicit, persisted pairing/auth; tier
endpoints as *native* (deep, synced) vs *bridged* (just-works). **Where we diverge:**
be open at the source layer — a documented provider API is our differentiator.

## How comparable open-source servers do it

| Project | Lang | Plugin model |
|---|---|---|
| **Music Assistant** | Python | `MusicProvider` + `PlayerProvider` classes; `supported_features: [ProviderFeature]`; two-phase playback `get_stream_details()` → `get_audio_stream()`; built-in providers shipped in the server |
| **Mopidy** | Python | A backend exposes `LibraryProvider` + `PlaybackProvider` + `PlaylistsProvider`; `translate_uri()` is often the only override; in-process |
| Navidrome | Go | WASM (sandboxed) plugins — for untrusted third-party code |
| Jellyfin | C# | Out-of-process assembly DLLs |
| Beets | Python | setuptools entry-point plugins |

Music Assistant is the closest blueprint: two provider kinds, per-provider capability
flags, providers compiled into the server.

## The plan: in-process, compiled-in Rust providers

Per the goal — "simple, open-source, compiled into the server" — we use enum-dispatch
traits gated by cargo features, not out-of-process/WASM (defer that until untrusted
plugins matter). This matches the existing `PlayerHandle` enum pattern (an enum, not
`dyn`, to keep async methods object-safe).

**Two trait families** (formalizing what already exists — `MusicProvider` +
`LocalDiskProvider`, and the `PlayerHandle`/`PlayerCapabilities` pair):

- `MusicProvider` (a *source*): `id` · `capabilities()` · `configure/start/stop` ·
  `health()` (Roon's Status analog) · `scan() -> Library` (library sources) ·
  `browse(path)` (non-scannable sources) · `resolve(id) -> StreamSpec` (MA's
  stream-details/stream split).
- `PlayerProvider` (an *endpoint*): `id` · `capabilities()` · `discover()` ·
  `execute(PlayerCommand)` · `subscribe() -> PlaybackState`.

**Mechanics:**
- Dispatch by **enum**, consistent with `PlayerHandle`; adding a provider = one variant
  + match arms.
- **Cargo features** (`provider-radio`, `player-airplay`, …) compile providers in/out;
  default = local disk + browser + MPD.
- A **`ProviderRegistry`** built at startup from an explicit list (one entry per
  feature). Explicit list beats macro auto-registration for a handful of providers;
  adopt `inventory`/`linkme` only if that list becomes friction.
- **Runtime config** decides which available providers are *active* (like `--mpd` and
  the persisted players table today).
- Keep the **trait as the stable boundary** so a future WASM/subprocess host can wrap
  it without rewriting providers.

**Roon-derived rules baked in:** Core authoritative; capability negotiation; source ≠
transport; explicit auth/pairing for networked endpoints (the M10 server↔player auth
task); endpoint tiering native vs bridged.

## Rollout

- **M9 (sources):** ① **Internet radio** — the first non-library provider (no scan;
  user-managed stations; `resolve()` returns the stream URL). Realized first as a
  working feature (storage + native API + Subsonic internet-radio endpoints + web UI +
  a `PlayStream` player command), then folded behind the formal `MusicProvider` trait.
  ② Formalize `MusicProvider` (capabilities/lifecycle/`resolve`) + the registry, with
  local disk as the reference impl.
- **M10 (players):** ③ re-express Browser + MPD behind `PlayerProvider`; ④ one bridged
  endpoint (Snapcast or AirPlay/Chromecast) as the "Roon Tested" tier.

References: Roon KB (RAAT, partner programs), `node-roon-api`, Music Assistant
developer docs, Mopidy backend API, the `inventory`/`linkme` crates.
